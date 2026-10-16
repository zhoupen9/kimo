//! The egui application: all UI state and drawing. Network results arrive as
//! `sync::Event`s drained at the top of every frame; opening a socket happens
//! only in worker threads.
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
//! worker thread. The pen is the compose card: a draft, and the Send button
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
use crate::models::{Account, Attachment, MailMessage, Protocol, SmtpLogin, SmtpSettings, TlsMode};
use crate::outgoing::{self, Outgoing, OutgoingAttachment};
use crate::search::{Scope, SearchHit, SearchQuery, SEARCH_LIMIT};
use crate::storage::{self, Settings};
use crate::sync::{self, Event, Job, SentCopy};
use crate::theme::{self, Accent, ColorScheme, Theme};
use crate::threads::{self, Thread};

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

/// What a row's own delete button says: the mail the row speaks for, and — for
/// a conversation — that it is only that one, since a conversation's row shows
/// its newest mail and the button takes the same mail the row opens in the
/// pane. The wording is the title bar's Delete, which takes the same mail.
fn row_delete_tooltip(place: RowPlace<'_>) -> &'static str {
    match place {
        RowPlace::Thread { .. } => {
            "Delete — removes the newest mail of this conversation from the server too"
        }
        RowPlace::Plain | RowPlace::InThread => MailAction::Delete.tooltip(),
    }
}

/// What a row's own flag button says: the title bar's flag cell wording for a
/// mail — which turns over with the state, as that cell's does — and, for a
/// conversation, the fact that it is the newest mail that takes the mark: the
/// same mail its delete button beside it takes.
fn row_flag_tooltip(place: RowPlace<'_>, flagged: bool) -> &'static str {
    match (place, flagged) {
        (RowPlace::Thread { .. }, false) => {
            "Flag — marks the newest mail of this conversation with a warning colour"
        }
        (RowPlace::Thread { .. }, true) => {
            "Unflag — takes the warning colour off the newest mail of this conversation"
        }
        (_, flagged) => MailAction::flag_tooltip(flagged),
    }
}

/// What the Delete cell says in select mode, where what it would delete is a
/// selection rather than the one mail the reading pane shows. The count is the
/// one thing the seven glyphs cannot say.
fn delete_selection_tooltip(count: usize) -> String {
    match count {
        1 => "Delete — removes the selected mail from the server too".to_string(),
        n => format!("Delete — removes the {n} selected mails from the server too"),
    }
}

/// What the message list's select-all box says: nothing checked is off,
/// everything the list holds is on, anything between is partial. The two
/// numbers are the whole selection's size and the list's own — comparable
/// because the selection is always a subset of the list on screen, which is
/// what cancel_select on every list change and the two guards in apply_event
/// keep true.
fn select_all_state(checked: usize, listed: usize) -> CheckState {
    match checked {
        0 => CheckState::Off,
        n if n >= listed => CheckState::On,
        _ => CheckState::Partial,
    }
}

/// The select-all row's label: the action a click would take, so the row says
/// what it is for without a tooltip having to be found.
fn select_all_label(state: CheckState) -> &'static str {
    match state {
        CheckState::On => "Clear selection",
        CheckState::Off | CheckState::Partial => "Select all",
    }
}

/// Where the select-all row sits while select mode is on: pinned under the
/// title bar, over the top of the list's own viewport — the list scrolls *under*
/// it, so the one control that is about the whole list never scrolls out of
/// reach. The frame's top padding is what keeps the first row out from under it
/// at rest.
fn select_all_band(viewport: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(viewport.left(), viewport.top() + TITLEBAR_HEIGHT),
        egui::vec2(viewport.width(), SELECT_ALL_HEIGHT),
    )
}

/// The select-all row's hover text, which is where the count lives — the row
/// itself is one line of chrome and stays one line whatever the list holds.
fn select_all_tooltip(state: CheckState, listed: usize) -> String {
    match (state, listed) {
        (CheckState::On, 1) => "Clear the selection — the mail in this list is checked".to_string(),
        (CheckState::On, n) => {
            format!("Clear the selection — all {n} mails in this list are checked")
        }
        (CheckState::Off | CheckState::Partial, 1) => "Select the mail in this list".to_string(),
        (CheckState::Off | CheckState::Partial, n) => format!("Select all {n} mails in this list"),
    }
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
    /// Whether the on-the-spot card has its Cc and Bcc lines folded out under
    /// To, which is what the chevron at the end of that row opens and closes.
    /// The popup always draws the two, so this only means something on the
    /// spot, and a card opens with them away: the answer under its mail is a
    /// letter, and the two lines are one click from it (`compose_to_row`).
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

/// The subject of a mail that answers or passes on another: the prefix,
/// unless the subject already carries one — `Re: Re:` helps nobody, and the
/// prefixes other clients write (`答复:`, `FWD:`) are prefixes all the same
/// (`threads::strip_subject_prefix`, the threading's own table).
fn answer_subject(prefix: &str, subject: &str) -> String {
    let subject = subject.trim();
    if subject.is_empty() || threads::strip_subject_prefix(subject).is_some() {
        subject.to_string()
    } else {
        format!("{prefix}: {subject}")
    }
}

/// Put one file on the draft, or say why it cannot be: what a drop on the card
/// and a path typed into its box both come to. The refusal — a path that is not
/// there, a directory — is kept in the draft for the card to draw beside the
/// box, and the mail is not sent with a part that cannot be read.
fn attach_file(draft: &mut ComposeDraft, path: PathBuf) -> bool {
    match OutgoingAttachment::of(path) {
        Ok(file) => {
            draft.attachments.push(file);
            draft.attach_error = None;
            true
        }
        Err(problem) => {
            draft.attach_error = Some(problem.to_string());
            false
        }
    }
}

/// The original mail as the bottom half of a reply or a forward: who wrote
/// it and when, and their text a line at a time. The compose card is plain
/// text — hard rule 4 — so the quote is plain text too, whatever the mail
/// itself was written in: the text part if it has one, the HTML rendered
/// down to words if it does not.
fn quoted_body(mail: &MailMessage) -> String {
    let plain = if mail.body_text.trim().is_empty() {
        mail.body_html
            .as_deref()
            .map(crate::mail::html_to_plain)
            .unwrap_or_default()
    } else {
        mail.body_text.clone()
    };
    let when = if mail.date.is_empty() {
        "an unknown date"
    } else {
        mail.date.as_str()
    };
    let mut out = format!("On {when}, {} wrote:\n", mail.from);
    for line in plain.lines() {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Why the compose window's Send cannot be pressed, or `None` when it can.
///
/// One place decides, and the worker checks the same rules again
/// (`sync::send_message`): a card that says "ready" about a mail that cannot
/// leave would fail at the socket instead of saying so.
fn send_blocker(
    account: Option<&Account>,
    to: &[String],
    cc: &[String],
    bcc: &[String],
) -> Option<String> {
    let Some(account) = account else {
        return Some("No account yet — add one in Preferences → Accounts.".to_string());
    };
    if !account.can_send() {
        return Some(
            "This account has no outgoing server — set one in Preferences → Accounts.".to_string(),
        );
    }
    // The account's username is what the mail would be sent *as*, so an
    // account whose username is not an address is one that cannot send.
    if !outgoing::is_address(&account.username) {
        return Some(format!(
            "{} is not an email address, so it cannot be the sender.",
            account.username
        ));
    }
    let invalid = to
        .iter()
        .chain(cc)
        .chain(bcc)
        .find(|addr| !outgoing::is_address(addr));
    if let Some(address) = invalid {
        return Some(format!("Not an address: {address}"));
    }
    if to.is_empty() && cc.is_empty() && bcc.is_empty() {
        return Some("Add at least one recipient.".to_string());
    }
    None
}

/// What the timed line above the message list says about a mail that went out:
/// who it went to, and what became of the copy in the Sent mailbox.
fn sent_notice(recipients: &[String], copy: &SentCopy) -> String {
    let who = recipients.join(", ");
    match copy {
        SentCopy::None => format!("Sent to {who}"),
        SentCopy::Saved { mailbox } => format!(
            "Sent to {who} · a copy is in {}",
            crate::protocol::imap::decode_mailbox_name(mailbox)
        ),
        SentCopy::Failed(reason) => {
            format!("Sent to {who} · the copy could not be saved: {reason}")
        }
    }
}

/// The notification one finished export raises: what happened, and where the
/// file went — the one thing about an export that is not known until the worker
/// has written it, since the dialog names the file and a session with no dialog
/// gets a name the writer numbered. `(summary, body)`, the two lines a desktop
/// notification shows, the way a batch of new mail's are (`new_mail_notices`).
///
/// An export is announced by the desktop and not by the line above the message
/// list (`sent_notice`, `AttachmentDownloaded`): its click was in the title
/// bar, and the file it wrote is not in the list.
fn exported_notification(path: &Path) -> (String, String) {
    (
        "Mail exported".to_string(),
        format!("Saved to {}", path.display()),
    )
}

/// How many new mails one batch announces one by one. Past this a single
/// notification counts them instead: a folder that catches up with thirty
/// arrivals must not stack thirty popups on the desktop.
const NOTIFY_MAX: usize = 3;

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

/// The notifications one batch of new mail raises: the mail itself when there
/// are few, one line counting them when there are many. `(summary, body)`, the
/// two lines a desktop notification shows.
fn new_mail_notices(folder: &str, arrivals: &[Arrival]) -> Vec<(String, String)> {
    if arrivals.len() <= NOTIFY_MAX {
        return arrivals
            .iter()
            .map(|arrival| (arrival.from.clone(), arrival.subject.clone()))
            .collect();
    }
    let body = arrivals
        .iter()
        .take(NOTIFY_MAX)
        .map(|arrival| format!("{}: {}", arrival.from, arrival.subject))
        .collect::<Vec<_>>()
        .join("\n");
    vec![(format!("{} new messages in {folder}", arrivals.len()), body)]
}

/// Whether new mail filed in a folder of this kind is announced. Not the
/// folders the user's own copies live in (Sent, Drafts) and not the ones mail
/// is filtered into rather than delivered to (Trash, Junk) — nor Gmail's All
/// Mail and Starred, where every mail the account holds turns up a second
/// time, so announcing them would double every arrival.
fn announces_new_mail(kind: FolderKind) -> bool {
    !matches!(
        kind,
        FolderKind::Sent
            | FolderKind::Drafts
            | FolderKind::Trash
            | FolderKind::Junk
            | FolderKind::AllMail
            | FolderKind::Starred
    )
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
    /// Accounts page: the add/edit account form replaces the account list
    /// while this is set.
    show_account_form: bool,
    account_form: AccountForm,
    confirm_delete: Option<String>,
    nav_collapsed: bool,
    /// The reading pane's width as its splitter left it — the width to
    /// bring the pane back to when a narrow window stops capping it
    /// (`plan_columns`): egui remembers only what a panel last rendered
    /// at, which while capped is the cap, so the uncapped width is the
    /// app's own to keep.
    pane_width: f32,
    /// Whether the pane is being capped this frame: egui's memory then
    /// holds the capped width, and `pane_width` above is what it is
    /// restored from the moment the cap lifts.
    pane_capped: bool,
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
    /// The message list's own rect this frame, set by `mail_list`. The rows'
    /// rules span it so they reach the column's hairlines; nothing else reads
    /// it.
    list_column: egui::Rect,
    /// How far the message list is scrolled, in points — zero when it has no
    /// scroll area at all. The title bar floats over the column once this
    /// leaves the top, and docks again when it comes back.
    list_scroll: f32,
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

/// Compare two display names the way a reader would: case-insensitively, so
/// "ada" and "Ada" sort together rather than one running before the other on a
/// byte value.
fn compare_names(one: &str, two: &str) -> std::cmp::Ordering {
    one.to_lowercase().cmp(&two.to_lowercase())
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

/// Whether a `To` header names one of the addresses: a substring test, the way
/// the search popup's own `To` filter matches (`LIKE %address%`), because what
/// is stored is the display form rather than a parsed list.
fn addressed_to(to: &str, me: &[String]) -> bool {
    let to = to.to_lowercase();
    me.iter()
        .any(|address| to.contains(&address.to_lowercase()))
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
            smtp_enabled: false,
            smtp_host: String::new(),
            smtp_port_str: SmtpSettings::default_port(TlsMode::Implicit).to_string(),
            smtp_tls: TlsMode::Implicit,
            smtp_own_login: true,
            smtp_username: String::new(),
            smtp_password: String::new(),
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
        }
    }

    /// Every required field is filled in (and the ports are ports). The
    /// outgoing server is required only while the account is meant to send.
    fn is_valid(&self) -> bool {
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
            smtp: self.smtp(),
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
            show_account_form: false,
            account_form: AccountForm::default(),
            confirm_delete: None,
            nav_collapsed: false,
            // The pane's remembered width starts at its default; the first
            // frame adopts whatever egui has stored, and every frame after
            // that follows the splitter (see `ui`).
            pane_width: CONTENT_DEFAULT_WIDTH,
            pane_capped: false,
            pane_popup: false,
            list_scrolls: false,
            collapsed_accounts: HashSet::new(),
            was_narrow: false,
            folder_icons,
            glass: Glass::default(),
            theme: Theme::parse(&settings.theme),
            // A file that names no accent, or one no accent answers to.
            accent: Accent::parse(settings.accent.as_deref().unwrap_or_default()),
            system_dark: system_prefers_dark(),
            storage_warning: crate::db::warning(),
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
            font_families: None,
            ui_font: settings.ui_font.clone(),
            mono_font: settings.mono_font.clone(),
            blitz_pages: HashMap::new(),
            blitz_order: Vec::new(),
            blitz_cancel: crate::blitz_render::Cancellation::default(),
            pending_blitz_key: None,
            blitz_paper: HashMap::new(),
            body: BodyCache::default(),
            scrolled_message: None,
            list_column: egui::Rect::ZERO,
            list_scroll: 0.0,
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
                new_mail,
            } => {
                // The worker has already stored this batch; what follows is
                // the in-memory copy the list and the reading pane draw from.
                // Whether the batch moved the folder onto a new UIDVALIDITY
                // generation is remembered for the selection below: the two
                // cannot be touched in the same borrow of the state.
                let mut rotated = false;
                // The mail of a batch the fetch found beyond the folder's mark
                // — the news a notification may be raised from. Collected here
                // and announced below, because the state is borrowed until
                // then.
                let mut arrivals: Vec<Arrival> = Vec::new();
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
                            rotated = true;
                        }
                        fc.uidvalidity = Some(uv);
                    }
                    for m in messages {
                        match fc.messages.iter().position(|x| x.uid == m.uid) {
                            Some(i) => {
                                let keep_seen = fc.messages[i].seen || m.seen;
                                let mut merged = m;
                                merged.seen = keep_seen;
                                // Labels are the user's own marks and were
                                // never in the fetched mail, so the copy the
                                // app holds is the one that stays — a POP3
                                // re-sync re-downloads every message it has.
                                merged.labels = std::mem::take(&mut fc.messages[i].labels);
                                fc.messages[i] = merged;
                            }
                            None => {
                                // Mail that is *news*: what the fetch found
                                // beyond the folder's mark, still unread, and
                                // not already here under this folder's own uid.
                                if new_mail && !m.seen {
                                    arrivals.push(Arrival::of(&m));
                                }
                                fc.messages.push(m);
                            }
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
                // The UIDs of the old generation are meaningless, and so is a
                // check made of one: the select-all box counts the selection
                // against the list, so it must never hold a mail the list no
                // longer has.
                if rotated {
                    self.selected_mails
                        .retain(|key| !(key.account == account_id && key.folder == folder));
                }
                self.announce_new_mail(&account_id, &folder, arrivals);
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
                //
                // A checked mail that went is not checked any more: its row is
                // about to leave the list, and the selection must not keep a
                // key nothing on screen stands for.
                self.selected_mails
                    .remove(&MailKey::new(&account_id, &folder, &uid));
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
            // An exported mail is announced by the desktop, not by a line above
            // the message list: the click was in the title bar and the file is
            // not in the list, so the notification is where the answer belongs —
            // and where the file went is the one thing about an export that is
            // not known until the worker has written it.
            //
            // The notification preference (Preferences → General) is not
            // consulted: it is a switch on mail that arrives by itself, and this
            // answers a click the user has just made. A click answered with
            // silence would be worse than one answered where they are not
            // looking.
            Event::MessageExported { path } => {
                let (summary, body) = exported_notification(&path);
                crate::notify::show(&summary, &body, self.ctx.pixels_per_point());
            }
            // A downloaded attachment keeps the line: that click was a card under
            // the mail, and the line is drawn beside what was clicked. Where the
            // file went is the one thing the app could not know when the work was
            // asked for, since a name that was taken is numbered.
            Event::AttachmentDownloaded { path } => {
                self.notice = Some((format!("Saved to {}", path.display()), Instant::now()));
            }
            Event::FilesChosen { files } => {
                // The dialog came back: the card is the one that asked, so the
                // files go on whatever draft is open — and nowhere if the card
                // was closed while the dialog was up, because the draft it was
                // asking for is gone with it.
                self.picking_files = false;
                for path in files {
                    if let Some(draft) = self.compose.as_mut() {
                        attach_file(draft, path);
                    }
                }
                // Empty is the dialog cancelled: no error, and nothing to say.
                self.ctx.request_repaint();
            }
            Event::FileChoiceFailed { error } => {
                self.picking_files = false;
                if let Some(draft) = self.compose.as_mut() {
                    draft.attach_error = Some(error);
                }
                self.ctx.request_repaint();
            }
            Event::MessageSent {
                recipients, copy, ..
            } => {
                // The server has the mail, so the card has nothing left to
                // hold: the draft goes with it, and the one line above the
                // message list says where the mail went.
                self.compose = None;
                self.notice = Some((sent_notice(&recipients, &copy), Instant::now()));
            }
            Event::MessageSendFailed { error, .. } => {
                // The draft is still the only copy of the mail, so the card
                // stays open with the server's own words in it — "550 5.1.1 no
                // such user" is the whole answer to why nothing went out — and
                // Send is enabled again.
                if let Some(draft) = self.compose.as_mut() {
                    draft.sending = false;
                    draft.error = Some(error);
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
            Event::SearchResults { seq, hits } => {
                // A result for a query the user has moved on from never
                // reaches the screen.
                if seq == self.search_seq {
                    self.search_hits = hits;
                    self.search_running = false;
                    self.search_error = None;
                    // A fresh answer is the whole of what the column shows, so
                    // a checked hit this one no longer names is not on screen
                    // any more — and the select-all box counts the selection
                    // against the hits, so it must not be able to count a mail
                    // that is not one. Only while a search is what the column
                    // shows: an answer for a query already cleared says nothing
                    // about a folder's selection.
                    if self.search.is_active() {
                        let live: HashSet<(&str, &str, &str)> = self
                            .search_hits
                            .iter()
                            .map(|hit| {
                                (
                                    hit.account_id.as_str(),
                                    hit.folder.as_str(),
                                    hit.uid.as_str(),
                                )
                            })
                            .collect();
                        self.selected_mails.retain(|key| {
                            live.contains(&(
                                key.account.as_str(),
                                key.folder.as_str(),
                                key.uid.as_str(),
                            ))
                        });
                    }
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

    /// Raise the desktop notifications for one batch of arrived mail: one per
    /// mail while there are few of them, one counting them when there are many
    /// (`new_mail_notices`). Nothing is raised when the preference is off, when
    /// the folder is one whose new mail is not announced (`announces_new_mail`),
    /// or for a mail the account already holds under another folder.
    ///
    /// The work is `crate::notify`'s and it never happens here: `show` only posts
    /// to the thread that talks to the notification daemon, so neither this
    /// frame nor a sync job ever waits for one.
    fn announce_new_mail(&self, account_id: &str, folder: &str, arrivals: Vec<Arrival>) {
        if arrivals.is_empty() || !self.notifications {
            return;
        }
        // The wire form is what the state and the database key a folder by;
        // what is announced, and what the folder is recognized as, is the
        // name a person reads.
        let display = crate::protocol::imap::decode_mailbox_name(folder);
        if !announces_new_mail(folders::kind_of(&display)) {
            return;
        }
        let elsewhere = self.message_ids_elsewhere(account_id, folder);
        let arrivals: Vec<Arrival> = arrivals
            .into_iter()
            .filter(|arrival| !elsewhere.contains(arrival.message_id.as_str()))
            .collect();
        let scale = self.ctx.pixels_per_point();
        for (summary, body) in new_mail_notices(&display, &arrivals) {
            crate::notify::show(&summary, &body, scale);
        }
    }

    /// The `Message-ID`s the account holds under any folder but this one — the
    /// one question that keeps a message its server files twice (Gmail's All
    /// Mail, a label beside the Inbox) from being announced twice. Gathered in
    /// one pass rather than asked per mail: a batch can hold hundreds of mails,
    /// and every answer is about the same folders. Mail cached before Kimo kept
    /// that header has no id, is not in the set, and is announced.
    fn message_ids_elsewhere<'a>(&'a self, account_id: &str, folder: &str) -> HashSet<&'a str> {
        self.states
            .get(account_id)
            .map(|state| {
                state
                    .mailbox
                    .folders
                    .iter()
                    .filter(|other| other.name != folder)
                    .flat_map(|other| other.messages.iter())
                    .map(|mail| mail.message_id.as_str())
                    .filter(|id| !id.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    // ----- actions ----------------------------------------------------------

    /// Every account's folder list and new mail — and, for the account whose
    /// folder is open, that folder's *first* fetch when it has none. The column
    /// says "Not fetched yet — click Sync in the title bar" for exactly that
    /// case, so the button has to honour it: the sweep only revisits folders
    /// that already carry a mark.
    fn sync_all(&mut self) {
        // The automatic sync's clock is measured from the last sweep, whoever
        // asked for it: pressing Sync is not a reason to be swept again a moment
        // later.
        self.auto_synced_at = Instant::now();
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

    /// The automatic sync: every `auto_sync_every`, and only while the
    /// preference is on — it is the one sync nobody asks for, and it is off
    /// unless it was asked for once. Nothing else asks for a frame minutes from
    /// now, so the wait is what keeps the clock ticking — a frame with nothing
    /// to draw is not a frame the app would otherwise be given.
    ///
    /// A sweep that is already in flight is left to finish (a second one would
    /// fetch the same mail twice); the clock is set either way, so a busy
    /// account is asked again a full interval later rather than every frame.
    fn pump_auto_sync(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let Some(wait) = auto_sync_wait(
            self.auto_sync,
            self.auto_synced_at,
            self.auto_sync_every,
            now,
        ) else {
            return;
        };
        if wait.is_zero() {
            self.auto_synced_at = now;
            if !self.sync_in_flight() {
                self.sync_all();
            }
        }
        ctx.request_repaint_after(wait);
    }

    /// Whether any account is syncing right now, which is what the automatic
    /// sync waits for.
    fn sync_in_flight(&self) -> bool {
        self.states.values().any(|state| state.syncing)
    }

    fn select_folder(&mut self, account_id: String, folder: String, protocol: Protocol) {
        self.selected_account = Some(account_id.clone());
        self.selected_folder = Some(folder.clone());
        self.selected_message = None;
        // The column is about to show another folder's mail, and a selection is
        // a selection of the list it was made on.
        self.cancel_select();

        let needs_fetch = match self.states.get(&account_id) {
            Some(state) => match state.mailbox.folders.iter().find(|f| f.name == folder) {
                Some(fc) => !fc.synced,
                None => true,
            },
            None => false,
        };
        // A folder nobody has fetched is fetched on the click that opens it;
        // until then the column says "Not fetched yet", which is also what the
        // title-bar Sync is for.
        if !needs_fetch {
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
        // Whatever was checked of it is not on screen any more.
        self.cancel_select();
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

    /// Export the mail the reading pane shows: a worker asks the desktop where
    /// to put it (`crate::portal::save_file`, the same XDG portal the attach
    /// card opens its file chooser with), fetches the mail's original source
    /// from the server and writes it there as a `.eml` (`Job::ExportMessage`).
    /// The file is the message whole — every header, every body part, every
    /// attachment — which is why the server is asked rather than the database
    /// read: what Kimo keeps is the parsed mail, with each attachment's name,
    /// type and size and never its bytes.
    ///
    /// Closing the dialog writes nothing and says nothing: cancelling a save is
    /// not a failure. A session with no dialog to open is not an export that
    /// cannot happen either — the file goes to `storage::downloads_dir()`,
    /// which is also the folder the dialog is suggested to open on, and the
    /// notification says which file it was.
    ///
    /// The round trip is the catch: mail the server no longer has cannot be
    /// exported at all, and that comes back on the account (`Event::SyncFailed`,
    /// prefixed "export:") like any other job's failure. What went right is a
    /// desktop notification naming the file (`Event::MessageExported`, through
    /// `exported_notification`) rather than a line above the message list: the
    /// click was in the title bar, and the file is not in the list.
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
                // Where the file goes when the desktop has no dialog to offer,
                // and the folder the dialog is suggested to open on.
                dir: storage::downloads_dir(),
                subject: msg.subject,
            },
            self.ctx.clone(),
            self.tx.clone(),
        );
    }

    /// Flag the mail the reading pane shows, or take its flag off: what the
    /// title bar's flag cell runs, and the one mail action that is about the
    /// *server* as much as about Kimo — `\Flagged` is a mark IMAP keeps, so
    /// the job behind this tells the server as well as the database
    /// (`Job::SetFlag`).
    ///
    /// The mail's own state is flipped first, in memory: the list's row and the
    /// pane's card are drawn from it, so the warning tint appears under the
    /// pointer rather than a round trip later. Nothing is said when the write
    /// fails — the flag is already on screen, and the account's failure is
    /// reported on the sync cell like every other one; the next load of the
    /// folder is where the two agree again.
    fn toggle_flag_on(&mut self, key: &MailKey) {
        let Some(account) = self
            .accounts
            .iter()
            .find(|account| account.id == key.account)
            .cloned()
        else {
            return;
        };
        // What the click is about: the flag the mail carries now, flipped.
        let Some(flagged) = self
            .folder_messages_of(&key.account, &key.folder)
            .and_then(|messages| messages.iter().find(|mail| mail.uid == key.uid))
            .map(|mail| !mail.flagged)
        else {
            return;
        };
        self.set_flag_on_mail(key, flagged);
        sync::spawn(
            Job::SetFlag {
                account,
                folder: key.folder.clone(),
                uid: key.uid.clone(),
                flagged,
            },
            self.ctx.clone(),
            self.tx.clone(),
        );
        self.ctx.request_repaint();
    }

    /// Flag the mail the reading pane shows, or take its flag off: what the
    /// title bar's flag cell runs, which is `toggle_flag_on` on the mail the
    /// pane has open.
    fn toggle_flag(&mut self) {
        if let Some(key) = self.selected_mail_key() {
            self.toggle_flag_on(&key);
        }
    }

    /// Delete what the title bar's Delete is about: the mail the reading pane
    /// shows, or — while the list is in select mode — every mail checked in it.
    ///
    /// Either way it is by way of a worker thread: the server is told first and
    /// each database row goes only once it has agreed (`Job::DeleteMessage`), so
    /// mail that is still on the server is never dropped here. The rows leave
    /// the list when the deletion comes back (`Event::MessageDeleted`), which is
    /// also where the reading pane is cleared if it was showing one of them.
    ///
    /// Nothing asks first: the button is the confirmation, and a deletion that
    /// cannot be undone is what the tooltip says it is.
    fn delete_selected_message(&mut self) {
        if self.select_mode {
            self.delete_checked_messages();
            return;
        }
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
        self.delete_mail(&account_id, &folder, &uid);
    }

    /// Delete one mail, by way of a worker thread: the mail the title bar's
    /// Delete has for the reading pane, and the mail a row's own delete button
    /// is about. The server is told first and the mail's database row goes only
    /// once it has agreed (`Job::DeleteMessage`), so mail that is still on the
    /// server is never dropped here; the row leaves the list when the deletion
    /// comes back (`Event::MessageDeleted`).
    ///
    /// Nothing asks first: the button is the confirmation, and a deletion that
    /// cannot be undone is what the tooltip says it is.
    fn delete_mail(&mut self, account_id: &str, folder: &str, uid: &str) {
        let Some(account) = self.accounts.iter().find(|a| a.id == account_id).cloned() else {
            return;
        };
        sync::spawn(
            Job::DeleteMessage {
                account,
                folder: folder.to_string(),
                uids: vec![uid.to_string()],
            },
            self.ctx.clone(),
            self.tx.clone(),
        );
    }

    /// Delete every checked mail. The selection is grouped by the folder the
    /// mails live in — a search can check hits from several accounts at once —
    /// and each group is one job on one connection (`delete_batches`).
    ///
    /// Nothing is cleared here: each row leaves the list when
    /// `Event::MessageDeleted` says the server agreed, and a mail the server
    /// refused keeps both its copies and its check, so the button is there to be
    /// pressed again.
    fn delete_checked_messages(&mut self) {
        for ((account_id, folder), uids) in delete_batches(&self.selected_mails) {
            let Some(account) = self.accounts.iter().find(|a| a.id == account_id).cloned() else {
                continue;
            };
            sync::spawn(
                Job::DeleteMessage {
                    account,
                    folder,
                    uids,
                },
                self.ctx.clone(),
                self.tx.clone(),
            );
        }
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
            self.cancel_select();
        } else if switches_mailbox && self.selected_account.as_deref() == Some(account_id.as_str())
        {
            // The folder list and its mail are about to be replaced.
            self.selected_folder = Some("INBOX".to_string());
            self.selected_message = None;
            self.cancel_select();
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
    /// with its id and a `/`, so `work` cannot match `work-2`) — and the papers
    /// its mails were switched to, which are no mail's any more.
    fn forget_blitz_pages(&mut self, account_id: &str) {
        self.forget_blitz_prefix(&format!("{account_id}/"));
        self.blitz_paper.retain(|key, _| key.account != account_id);
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

    /// Which paper this mail's Blitz page is drawn on: white unless the reader
    /// has switched this one to the dark surface.
    fn paper_of(&self, key: &MailKey) -> Paper {
        self.blitz_paper.get(key).copied().unwrap_or_default()
    }

    /// Switch one mail's paper between white and the dark surface. Nothing else
    /// depends on it — not the render, whose key has no paper in it, and not the
    /// mail, whose own background the renderer paints over this either way — so
    /// the click only has to ask for the next frame.
    fn toggle_blitz_paper(&mut self, key: &MailKey) {
        match self.paper_of(key) {
            Paper::White => {
                self.blitz_paper.insert(key.clone(), Paper::Dark);
            }
            Paper::Dark => {
                self.blitz_paper.remove(key);
            }
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
        // The paper is the pane's, not the page's: the render key has no paper
        // in it, so switching one repaints this frame and starts no work.
        let paper = self
            .pane_mail_key(&msg.uid)
            .map(|key| self.paper_of(&key))
            .unwrap_or_default();
        match self.blitz_pages.get(&key) {
            Some(BlitzPage::Ready(page)) => draw_blitz_page(ui, page, paper.color()),
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
            accent: Some(self.accent.key.to_string()),
            html_renderer: Some(self.html_renderer.as_str().to_string()),
            ui_font: self.ui_font.clone(),
            mono_font: self.mono_font.clone(),
            threaded: Some(self.threaded),
            auto_sync: Some(self.auto_sync),
            auto_sync_minutes: Some((self.auto_sync_every.as_secs() / SECS_PER_MINUTE) as u32),
            notifications: Some(self.notifications),
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
    /// toggle, the compose button, the account status, the search box, then the
    /// group of mail actions — and, at the window's own right end, the sync
    /// cell, the preferences cog and minimize / maximize / close. The leftover
    /// space drags the window (double-click toggles maximize).
    ///
    /// The bar is laid out as groups, not as one long row of widgets: the
    /// first group at the left — the nav toggle and the pen, which never give
    /// way — the sync cell, the cog and the window controls at the
    /// right, and the search box and the mail actions anchored to the columns
    /// under them (`titlebar_rects`, `titlebar_anchors`).
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
        // Nav toggle, compose. Laid out from the bar's left edge, and where it
        // came to is what every other group is placed after.
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
                    // The bar's own left margin, before the first cell: the
                    // toggle is the leftmost thing in the bar.
                    row.add_space(TITLEBAR_LEFT_INSET);

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
                    // Sync is not one of these cells: it belongs with the cog
                    // and the window controls at the right end (see below),
                    // because it is about the whole app rather than about the
                    // mail the pane shows.

                    row.min_rect().right()
                },
            )
            .inner;

        // --- the list's search box and the pane's mail actions ---
        // The bar's two anchored groups (`titlebar_anchors`): the box sits at
        // the message list's own left edge and the run at the reading pane's, so
        // each is over the column it belongs to and follows that column's
        // splitter. The first group is the one thing between them that never
        // gives way — the compose button needs no mail open — and both anchors
        // are clamped clear of it.
        let pane_left = if self.pane_popup {
            0.0
        } else {
            self.list_column.right()
        };
        let anchors = titlebar_anchors(
            bar,
            left_end,
            self.list_column.left(),
            pane_left,
            rects.sync.left(),
        );
        // The room the box has: up to whatever stands next to it on the right —
        // the pane's run, or the window controls when the run is not drawn.
        // (What the box leaves there is the filter cell's room, and the plan has
        // already decided whether either of them is drawn.)
        let next = anchors
            .actions
            .map_or(rects.sync.left(), |actions| actions.left());
        let mut box_end: Option<f32> = None;
        if let Some(box_width) = anchors.search_width {
            let middle = egui::Rect::from_min_max(
                egui::pos2(anchors.search, bar.top()),
                egui::pos2(anchors.search + box_width, bar.bottom()),
            );
            // Search, as one box: the query with the magnifier inside
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
            box_end = Some(box_rect.right());
        }

        // --- the filter and sort cells: the search box's own companions ---
        // The column's two view controls, in the box's group rather than with the
        // pane's actions: they are about what the list *is* — which of its mail
        // is shown, and in what order — which is what the box is about too. Each
        // says its state the way the magnifier does, taking the accent while it
        // is doing something other than its default, and each opens its own menu
        // of two or three groups of choices.
        if let Some(filter) = anchors.filter {
            ui.scope_builder(
                egui::UiBuilder::new()
                    .max_rect(filter)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                |row| {
                    clip_to(row, filter);
                    let response = titlebar_button(row, true);
                    // The accent while any view is ticked, the bar's text colour
                    // while none is — the way the magnifier says a query is on.
                    let color = if self.list_filters.is_all() {
                        visuals.text_color()
                    } else {
                        scheme.accent
                    };
                    paint_filter_icon(row.painter(), response.rect, color, MAIL_ACTION_GLYPH);
                    let response = response
                        .on_hover_text(format!("Filter — showing {}", self.list_filters.summary()));
                    self.filter_popup(&response);
                },
            );
        }
        if let Some(sort) = anchors.sort {
            ui.scope_builder(
                egui::UiBuilder::new()
                    .max_rect(sort)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                |row| {
                    clip_to(row, sort);
                    let response = titlebar_button(row, true);
                    // The accent while the list is ordered by anything but the
                    // default: the sort is always doing something, so what the
                    // colour says is that the user asked for it.
                    let color = if self.list_sort == ListSort::default() {
                        visuals.text_color()
                    } else {
                        scheme.accent
                    };
                    paint_sort_icon(row.painter(), response.rect, color, MAIL_ACTION_GLYPH);
                    let response =
                        response.on_hover_text(format!("Sort — {}", self.list_sort.summary()));
                    self.sort_popup(&response);
                },
            );
        }

        // --- the pane's mail actions ---
        // The pane's own row, drawn only while there is a mail to act on, or a
        // selection — which is what Delete takes, and the one action that stays
        // live in select mode. Seven dead glyphs over a pane with no mail in it
        // said nothing and cluttered the bar; the row comes and goes over
        // `MAIL_ACTION_FADE_TIME` rather than blinking, because the mail being
        // opened or closed is what the bar is following. The cells and glyphs are
        // the bar's own, so the pane's card head — the same five cells brought
        // down onto the mail — reads as this row's other half.
        if let Some(actions) = anchors.actions {
            let wanted = self.has_selected_message() || !self.selected_mails.is_empty();
            let shown = ui.ctx().animate_bool_with_time(
                egui::Id::new("titlebar-actions"),
                wanted,
                MAIL_ACTION_FADE_TIME,
            );
            if shown > 0.0 {
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(actions)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |row| {
                        clip_to(row, actions);
                        self.titlebar_mail_actions(row, &visuals, shown);
                    },
                );
            }
        }

        // --- drag surface: the chrome the bar has left ---
        // The window is undecorated, so this is what moves it (and what
        // double-click maximizes it). With the groups anchored at both ends —
        // the first group at the left, the list's box and the pane's run at
        // their columns' edges — the bar has two stretches of it: the chrome
        // over the navigation column, between the first group and the list's
        // box, and the room between the box and the pane's run (or the window
        // controls, when no run is drawn). A bar with no room to spare simply
        // has none — the window controls have the window's right end either way.
        let mut free: Vec<(f32, f32)> = Vec::new();
        match box_end {
            // The box is drawn: two stretches, one on each side of it.
            Some(end) => {
                free.push((left_end, anchors.search));
                // The box and the cells beside it are one group, so what is left
                // of the bar starts after whichever of them ends last.
                let group_end = anchors
                    .sort
                    .or(anchors.filter)
                    .map_or(end, |cell| cell.right());
                free.push((group_end, next));
            }
            // No box — not even a narrow one: one stretch from the first group
            // to whatever comes next.
            None => free.push((left_end, next)),
        }
        for (index, (from, to)) in free.iter().enumerate() {
            if to - from <= 2.0 * TITLEBAR_GROUP_GAP {
                continue;
            }
            let rect = egui::Rect::from_min_max(
                egui::pos2(from + TITLEBAR_GROUP_GAP, bar.top()),
                egui::pos2(to - TITLEBAR_GROUP_GAP, bar.bottom()),
            );
            let drag = ui.interact(
                rect,
                egui::Id::new(("titlebar-drag", index)),
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

        // --- right group: sync, the cog and the window controls ---
        // Fixed, and drawn last: they are what the rest of the bar is laid out
        // around, and the one thing a bar of any width keeps. The group's rect
        // starts at the sync cell, so the clip is also what says which half of
        // the bar a shape belongs to (see the no-overlap test in `tests`).
        let right = egui::Rect::from_min_max(egui::pos2(rects.sync.left(), bar.top()), bar.max);
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(right)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |row| {
                clip_to(row, right);
                // Sync now: the whole app's mail, from every account, on demand
                // — what the app does at startup and what the automatic sync
                // does on its clock, asked for by hand. The cell is the bar's
                // own, placed at the exact rect the arithmetic carved for it
                // like the cog and the window controls.
                //
                // **It is also where the app's sync state is shown**: a spinner
                // while any account is syncing, a dot while one's last sync
                // stands failed, and the accounts' own words in the tooltip.
                // That is what the strip beside it used to say, one letter at a
                // time; the cell is the better place for it — it is the control
                // a sweep is asked for from, it is always in the same place, and
                // nothing else in the bar has to move aside for it.
                let mut tooltip = String::from(
                    "Sync now — every account is asked for its mail, and the folder you have \
                     open is fetched too if it was never fetched.",
                );
                let mut failed = false;
                for account in &self.accounts {
                    let Some(state) = self.states.get(&account.id) else {
                        continue;
                    };
                    if state.syncing {
                        tooltip.push_str(&format!("\n{}: syncing…", account.name));
                    }
                    if let Some(error) = &state.error {
                        failed = true;
                        tooltip.push_str(&format!("\n{}: {error}", account.name));
                    }
                }
                let syncing = self.sync_in_flight();
                let sync_resp = titlebar_hover(
                    row,
                    row.interact(
                        rects.sync,
                        egui::Id::new("titlebar-sync"),
                        egui::Sense::click(),
                    ),
                );
                if syncing {
                    row.put(
                        rects.sync,
                        egui::Spinner::new()
                            .color(scheme.accent)
                            .size(SYNC_SPINNER_SIZE),
                    );
                } else {
                    paint_sync_icon(row.painter(), rects.sync, visuals.text_color());
                }
                if failed {
                    row.painter().circle_filled(
                        egui::pos2(rects.sync.right() - 7.0, rects.sync.top() + 7.0),
                        SYNC_BADGE_RADIUS,
                        scheme.danger,
                    );
                }
                if sync_resp.clicked() {
                    self.sync_all();
                }
                sync_resp.on_hover_text(tooltip);

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
                    // `TITLEBAR_RIGHT_MARGIN` short of the window's corner.
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
    /// pointer. **Select mode is the other answer**: the list is what the click
    /// is about then, and Delete — the one action that has something to do with
    /// more than one mail — is the only one of the seven left enabled
    /// (`mail_action_enabled`).
    ///
    /// Every cell is one of the bar's buttons — `TITLEBAR_BUTTON_SIZE` square,
    /// `TITLEBAR_BUTTON_GAP` from the next one, empty at rest and filled under
    /// the pointer with a small rounded corner (`titlebar_button`) — which is
    /// what keeps the seven of them reading as one group rather than as seven
    /// framed widgets. They are the same cells the nav toggle, the compose and
    /// sync buttons, the cog and the window controls are; what makes this run a
    /// group of its own is the clear chrome in front of it
    /// (`TITLEBAR_GROUP_GAP`), not a size or a fill only it has.
    ///
    /// Export and Delete are wired: Export hands the mail to
    /// `export_selected_message`, which has a worker write the server's own
    /// copy of it out as a `.eml`, and Delete hands it to
    /// `delete_selected_message`, which takes it off the server before it
    /// leaves the database. The other four are still drawn only — a click on
    /// one runs nothing, so they stay UI until the actions they name exist.
    fn titlebar_mail_actions(&mut self, row: &mut egui::Ui, visuals: &egui::Visuals, fade: f32) {
        // The run lays itself out: `TITLEBAR_BUTTON_GAP` between the cells, which
        // is the gap `titlebar_cells_width` assumes.
        row.spacing_mut().item_spacing.x = TITLEBAR_BUTTON_GAP;
        // The one action with a state of its own: a flagged mail's cell says so
        // in the scheme's warning colour — the same tint the mail's row and card
        // are filled with — rather than only taking a click.
        let flagged = self.selected_mail().is_some_and(|mail| mail.flagged);
        for action in MailAction::ALL {
            let enabled = self.mail_action_enabled(action);
            let response = titlebar_button_faded(row, enabled, fade);
            let color = if enabled && action == MailAction::Flag && flagged {
                crate::theme::of(row.ctx()).warn
            } else if enabled {
                visuals.text_color()
            } else {
                visuals.weak_text_color()
            }
            .gamma_multiply(fade);
            let painter = row.painter();
            match action {
                MailAction::Reply => {
                    paint_reply_icon(painter, response.rect, color, MAIL_ACTION_GLYPH)
                }
                MailAction::ReplyAll => {
                    paint_reply_all_icon(painter, response.rect, color, MAIL_ACTION_GLYPH)
                }
                MailAction::Forward => {
                    paint_forward_icon(painter, response.rect, color, MAIL_ACTION_GLYPH)
                }
                MailAction::Label => {
                    paint_label_icon(painter, response.rect, color, MAIL_ACTION_GLYPH)
                }
                MailAction::Flag => {
                    paint_flag_icon(painter, response.rect, color, MAIL_ACTION_GLYPH)
                }
                MailAction::Export => paint_export_icon(painter, response.rect, color),
                MailAction::Delete => {
                    paint_delete_icon(painter, response.rect, color, MAIL_ACTION_GLYPH)
                }
            }
            let response = response.on_hover_text(if enabled {
                if self.select_mode && action == MailAction::Delete {
                    delete_selection_tooltip(self.selected_mails.len())
                } else if action == MailAction::Flag {
                    // The click takes the flag off now, so the hover text says
                    // that rather than what a flag is.
                    MailAction::flag_tooltip(flagged).to_string()
                } else {
                    action.tooltip().to_string()
                }
            } else if self.select_mode {
                if action == MailAction::Delete {
                    "Delete — no mail selected".to_string()
                } else {
                    format!("{} — not available in select mode", action.label())
                }
            } else {
                format!("{} — no mail selected", action.label())
            });
            // The label cell carries the popup rather than running one action
            // (`label_popup`): picking several labels is what it is for, so the
            // menu stays open across the clicks in it, and it is drawn on every
            // frame the cell is. The box starts empty for each mail it is opened
            // on: what was typed for the last one is not a draft for this.
            if enabled && action == MailAction::Label {
                if let Some(key) = self.selected_mail_key() {
                    if response.clicked() {
                        self.label_draft.clear();
                    }
                    self.label_popup(&response, &key);
                }
            } else if enabled && response.clicked() {
                match action {
                    MailAction::Export => self.export_selected_message(),
                    MailAction::Delete => self.delete_selected_message(),
                    // The bar's three answers to a mail come up as popups
                    // over everything (`ComposeSpot::Popup`); the mail's own
                    // header opens the same three on the spot, in the pane
                    // the mail is in. The bar answers the mail the pane is
                    // showing — the header, the mail it sits on.
                    MailAction::Reply | MailAction::ReplyAll | MailAction::Forward => {
                        if let Some(mail) = self.selected_mail().cloned() {
                            self.compose_about(action, ComposeSpot::Popup, &mail);
                        }
                    }
                    MailAction::Label => {}
                    MailAction::Flag => self.toggle_flag(),
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
                // Always the column's last group: the labels belong to no
                // account, so they cannot be filed under one.
                self.label_group(ui, scheme);
            });
    }

    /// The navigation column's last group: the user's labels, under every
    /// account and its folders, drawn like folders because a label is a
    /// first-class filter beside them. A selected one carries the selection
    /// fill, and the message list shows the mail that carries **all** of the
    /// selected ones (`KimoApp::toggle_label`).
    fn label_group(&mut self, ui: &mut egui::Ui, scheme: ColorScheme) {
        // Derived here for the same reason the accounts' rows are: the frame
        // draws, and only a change derives.
        self.refresh_label_view();
        let Some(view) = &self.label_view else {
            return;
        };
        if view.rows.is_empty() {
            return;
        }
        let mut clicked: Option<String> = None;
        ui.add_space(LABEL_GROUP_GAP);
        label_group_header(ui, scheme);
        ui.indent("label-rows", |ui| {
            for row in &view.rows {
                if label_row(ui, scheme, &row.name, row.count, row.selected).clicked() {
                    clicked = Some(row.name.clone());
                }
            }
        });
        if let Some(name) = clicked {
            self.toggle_label(&name);
        }
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

    /// Middle column: the search results or the selected folder's mail, on the
    /// card the column is — the lighter content surface, framed by a hairline
    /// down each side.
    ///
    /// The card is as tall as what the list holds and no taller, so a folder
    /// with three mails leaves the chrome below them instead of a column of
    /// empty surface. A column with nothing to show — no account, an empty
    /// folder, a search with no hits — has no card at all, the way the reading
    /// pane has none with no mail open. How tall the card is is the list's own
    /// content, which is known only once the list has been laid out: the shape
    /// is reserved here, drawn before the rows, and filled in at the foot of
    /// this function.
    fn mail_list(&mut self, ui: &mut egui::Ui) {
        let scheme = theme::of(ui.ctx());
        // The column's own rect is the whole ui the frame hands over — the
        // padding moved inside the scroll area. The rows' rules span it (see
        // message_card), and the card's hairlines frame it.
        let column = ui.max_rect();
        self.list_column = column;
        let where_to_put_card = ui.painter().add(egui::Shape::Noop);
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
            .notice
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= NOTICE_TIME)
        {
            self.notice = None;
        } else if let Some((_, at)) = &self.notice {
            ui.ctx().request_repaint_after(NOTICE_TIME - at.elapsed());
        }
        if let Some((notice, _)) = &self.notice {
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
        // Three lists can stand in the column, and in this order: a query's
        // hits, a label filter's mail, or the folder's own. A query and a label
        // filter are two answers to what the column shows, and
        // `KimoApp::toggle_label` is where one gives way to the other.
        let listed = if self.search.is_active() {
            self.search_results(ui)
        } else if !self.label_filter.is_empty() {
            self.label_results(ui)
        } else {
            self.folder_messages(ui)
        };
        // The card goes in where it was reserved: under the rows, as tall as
        // they turned out to be. Nothing listed leaves the reserved shape a
        // `Noop`, which is the bare chrome the placeholders stand on.
        ui.painter()
            .set(where_to_put_card, list_card(column, listed, scheme));
    }

    /// Open the search popup with the keyboard already in its text field.
    fn open_search(&mut self) {
        self.show_search = true;
        self.search_focus = true;
    }

    /// Open the compose window on an empty draft. A window that is already open
    /// is left alone: the button is not a reset, and the draft in it is the
    /// only copy of what has been typed.
    /// Files the desktop dropped on the window this frame go on the mail being
    /// written — and a drop with nothing being written starts a card with them
    /// on it, which is what dropping a file on a mail client means.
    ///
    /// The *window's* input rather than the card's, because a drop belongs to
    /// the frame and this is the one place every frame passes through: read
    /// inside the card, a drop with no card open would land nowhere at all.
    /// (On a Wayland session no drop ever arrives — winit 0.30 has no
    /// drag-and-drop there — so the card's dialog button is the one way in that
    /// always works; see `crate::portal`.)
    fn attach_dropped(&mut self) {
        let dropped: Vec<PathBuf> = self.ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }
        self.open_compose();
        if let Some(draft) = self.compose.as_mut() {
            for path in dropped {
                attach_file(draft, path);
            }
        }
    }

    fn open_compose(&mut self) {
        if self.compose.is_none() {
            self.compose = Some(ComposeDraft::new(compose_from(
                &self.accounts,
                self.selected_account.as_deref(),
            )));
            self.compose_spot = ComposeSpot::Popup;
            self.compose_anchor = None;
        }
    }

    /// Open a compose about `mail` — from the title bar, as a popup over
    /// everything (`ComposeSpot::Popup`), or from the mail's own header, on
    /// the spot in the pane (`ComposeSpot::Pane`, the card right under that
    /// mail). The header's cells pass the mail they sit on; the bar's pass
    /// the one the pane is showing. A draft already on its way out is left
    /// alone: its Send is in flight, and the event that closes it is owed
    /// the draft it knows. Any other draft is replaced — the click is the
    /// user's, and the new one is what they asked for. The anchor records
    /// which mail the draft answers (`compose_anchor`): the pane puts its
    /// card right under that mail, and `ui` moves the draft to the popup
    /// window once that mail has left the pane.
    fn compose_about(&mut self, action: MailAction, spot: ComposeSpot, mail: &MailMessage) {
        if self.compose.as_ref().is_some_and(|draft| draft.sending) {
            return;
        }
        let from = compose_from(&self.accounts, self.selected_account.as_deref());
        // The one address a reply all must not put in Cc: the account's own.
        let self_addr = from
            .as_ref()
            .and_then(|id| self.accounts.iter().find(|account| &account.id == id))
            .map(|account| account.username.clone())
            .unwrap_or_default();
        let draft = match action {
            MailAction::Reply => ComposeDraft::reply(mail, from),
            MailAction::ReplyAll => ComposeDraft::reply_all(mail, from, &self_addr),
            MailAction::Forward => ComposeDraft::forward(mail, from),
            _ => return,
        };
        self.compose_anchor = self
            .selected_account
            .as_deref()
            .zip(self.selected_folder.as_deref())
            .map(|(account, folder)| MailKey::new(account, folder, &mail.uid));
        self.compose = Some(draft);
        self.compose_spot = spot;
    }

    /// The mail the reading pane shows, for the actions about it — the title
    /// bar's and the header's. A uid the folder no longer holds is no mail
    /// at all, which is `has_selected_message`'s own question.
    fn selected_mail(&self) -> Option<&MailMessage> {
        let uid = self.selected_message.as_deref()?;
        self.current_folder()?
            .messages
            .iter()
            .find(|mail| mail.uid == uid)
    }

    /// Whether the mail a pane compose answers is on the pane: part of the
    /// conversation it shows, in the account and folder it shows them of. A
    /// uid on its own would not say it — uids are a folder's own numbering,
    /// and another folder has uids of its own — so the account and the
    /// folder say it too.
    fn compose_anchor_on_pane(&self) -> bool {
        let Some(anchor) = self.compose_anchor.as_ref() else {
            return false;
        };
        if self.selected_account.as_deref() != Some(anchor.account.as_str())
            || self.selected_folder.as_deref() != Some(anchor.folder.as_str())
        {
            return false;
        }
        self.open_conversation().is_some_and(|conversation| {
            conversation
                .messages
                .iter()
                .any(|mail| mail.uid == anchor.uid)
        })
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

            let text = ui.add(Field::new(
                egui::TextEdit::singleline(&mut self.search.text)
                    .hint_text("Search all mail…")
                    .desired_width(f32::INFINITY),
            ));
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

    /// Send the draft the compose card holds: the mail goes to a worker as a
    /// `Job::SendMessage`, and the card waits for the answer with its boxes and
    /// buttons disabled (`ComposeDraft::sending`).
    ///
    /// What closes the card is `Event::MessageSent`, not this: what has been sent
    /// is the mail the server accepted, and until it says so the draft is the
    /// only copy of it.
    fn send_draft(&mut self) {
        let Some(draft) = self.compose.as_ref() else {
            return;
        };
        let account = self
            .accounts
            .iter()
            .find(|account| Some(&account.id) == draft.from.as_ref())
            .cloned();
        let (to, cc, bcc) = draft.recipients();
        // The button is drawn disabled in exactly this case; a frame drawn
        // before the last keystroke must not turn that into a send the worker
        // would refuse anyway.
        if send_blocker(account.as_ref(), &to, &cc, &bcc).is_some() {
            return;
        }
        let Some(account) = account else {
            return;
        };
        // The account's display name is the sender's name and its username the
        // address: the same pair the From list shows.
        let mail = Box::new(Outgoing {
            from_name: account.name.clone(),
            from_addr: account.username.clone(),
            to,
            cc,
            bcc,
            subject: draft.subject.clone(),
            body: draft.body.clone(),
            // The paths, not the bytes: the worker reads them as it sends
            // (`outgoing::load_attachments`), so the UI thread never touches a
            // file.
            attachments: draft.attachments.clone(),
        });
        if let Some(draft) = self.compose.as_mut() {
            draft.sending = true;
            draft.error = None;
        }
        sync::spawn(
            Job::SendMessage { account, mail },
            self.ctx.clone(),
            self.tx.clone(),
        );
    }

    /// The compose window, opened from the title bar: an `egui::Modal`, so the
    /// columns behind it are covered by a backdrop and cannot be used while a
    /// mail is being written.
    ///
    /// It edits a draft and sends it. Send hands the draft to a worker
    /// (`send_draft`, `Job::SendMessage`) and the card waits for the answer
    /// with its boxes and buttons disabled: `Event::MessageSent` is what closes
    /// it — the mail is out, and the draft has no other home — while
    /// `Event::MessageSendFailed` puts the server's own words in the card and lets
    /// the draft be sent again.
    ///
    /// The backdrop click and Escape deliberately do not close it: the draft is
    /// the only copy of the mail, and a click beside the card must not be what
    /// throws it away, so `ModalResponse::should_close` is ignored and Discard
    /// is the way out.
    fn compose_window(&mut self, ctx: &egui::Context) {
        let Some(heading) = self.compose.as_ref().map(|draft| draft.kind.label()) else {
            return;
        };
        let mut outcome = ComposeOutcome::Shown;
        let _ = egui::Modal::new(egui::Id::new("compose")).show(ctx, |ui| {
            ui.set_width(COMPOSE_WIDTH);
            ui.heading(heading);
            ui.separator();
            outcome = self.compose_card(ui, COMPOSE_WIDTH, ComposeSpot::Popup);
        });
        self.settle_compose_outcome(outcome);
    }

    /// What the card's own buttons said, wherever the card was drawn: Discard
    /// closes the draft, Send hands it to a worker, and nothing is nothing.
    fn settle_compose_outcome(&mut self, outcome: ComposeOutcome) {
        match outcome {
            ComposeOutcome::Discard => self.compose = None,
            ComposeOutcome::Send => self.send_draft(),
            ComposeOutcome::Shown => {}
        }
    }

    /// The compose card — the From list, the address fields, the body, the
    /// footer — drawn into `ui` at `width` points. The popup's window
    /// (`compose_window`) and the pane's card under the mail it answers
    /// (`compose_inline_card`) are one card drawn by one function, whose two
    /// shapes `spot` tells apart (`inline`): the shorter form under its mail,
    /// and the form of boxes in the window.
    ///
    /// The draft is taken out for the card — the closure edits it while
    /// reading the account list — and put back unless the card was
    /// discarded. Send is returned for the caller to act on: the draft is
    /// back in place by then, which is what `send_draft` reads.
    fn compose_card(&mut self, ui: &mut egui::Ui, width: f32, spot: ComposeSpot) -> ComposeOutcome {
        let Some(mut draft) = self.compose.take() else {
            return ComposeOutcome::Shown;
        };
        // The card under the mail it answers, rather than the popup over
        // everything: the two differ in what they show, and the container is
        // what says which one this is (`ComposeSpot`).
        let inline = spot == ComposeSpot::Pane;
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
        // The account the mail would go out from, whole: whether it *can* send
        // is a question about the account, not about the list of them.
        let sender = draft
            .from
            .as_ref()
            .and_then(|id| self.accounts.iter().find(|account| account.id == *id))
            .cloned();
        // A draft is edited freely; one that is already on its way out is not
        // touched at all.
        let sending = draft.sending;
        // The card's first frame, and what its keyboard follows: the popup's
        // form starts at To — a new mail and a forward both have a recipient
        // to name first — while an answer written on the spot already knows
        // who it goes to (a Reply and a Reply all both arrive addressed), so
        // its keyboard goes to the body, where the reply itself is written.
        let first = std::mem::take(&mut draft.focus);
        let body_first = inline && !draft.to.trim().is_empty();

        let mut discard = false;
        let mut send = false;
        // What the card's attachment rows ask for, applied once it has been
        // drawn: one file taken back off, and whether the card's own area was
        // clicked for the desktop's dialog. The files row settles both, in one
        // container or the other (`inline`), so they are written exactly once.
        let remove: Option<usize>;
        let pick: bool;
        {
            // The card is laid out at the caller's width, so the boxes get a
            // constant: what the label column and the gap beside it leave.
            let field_width = width - COMPOSE_LABEL_WIDTH - ui.spacing().item_spacing.x;
            // One row of the card's form: the label in the card's own column,
            // and the field — in Kimo's box, which is where a focused field
            // takes the accent (`Field`).
            let field_row =
                |ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str, flat: bool| {
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [COMPOSE_LABEL_WIDTH, COMPOSE_FIELD_HEIGHT],
                            egui::Label::new(label).halign(egui::Align::RIGHT),
                        );
                        let edit = egui::TextEdit::singleline(value)
                            .hint_text(hint)
                            .desired_width(field_width);
                        let field = Field::new(edit).enabled(!sending);
                        ui.add(if flat { field.flat() } else { field })
                    })
                    .inner
                };
            // The same row where the card is a letter rather than a form: flat,
            // the way the From list above it is drawn, without an outline or a
            // fill. Cc and Bcc are drawn with it when the To row's chevron has
            // folded the two out.
            let flat_field = |ui: &mut egui::Ui, label: &str, value: &mut String| {
                field_row(ui, label, value, "name@example.com", true)
            };
            let field = |ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str| {
                field_row(ui, label, value, hint, false)
            };

            // The From line, drawn wherever the card wants it: one row of the
            // popup's form, labelled, and — on the spot — the card's own head
            // bar, with the avatar of the account the answer goes from in the
            // place the label takes.
            //
            // `flat` takes the combo's box off for that head: an outline and a
            // fill belong to a form of boxes, and this one is the reply's own
            // from — a list that opens, not a field to fill in. Only its hover
            // keeps a fill, which is what still says it can be opened.
            let fallback_from = compose_from(&self.accounts, self.selected_account.as_deref());
            let from_list = |ui: &mut egui::Ui, from: &mut Option<String>, flat: bool| {
                let label = from
                    .as_ref()
                    .and_then(|id| accounts.iter().find(|(account_id, ..)| account_id == id))
                    .map(|(_, name, address)| format!("{name} <{address}>"));
                match (accounts.is_empty(), label) {
                    (true, _) => {
                        ui.label(
                            egui::RichText::new(
                                "No account yet — add one in Preferences → Accounts.",
                            )
                            .weak(),
                        );
                    }
                    (false, Some(label)) => {
                        ui.add_enabled_ui(!sending, |ui| {
                            if flat {
                                flatten_widgets(ui, theme::of(ui.ctx()).hover);
                            }
                            // What egui's own icon hook is handed — where the
                            // filled triangle would go, and whether the list
                            // is open — is kept here for the chevron painted
                            // after the button: the pointer's state, which
                            // that chevron's colour answers, is known only
                            // then (`paint_from_chevron`).
                            let icon = std::rc::Rc::new(std::cell::Cell::new((
                                egui::Rect::NOTHING,
                                false,
                            )));
                            let mut combo = egui::ComboBox::from_id_salt("compose-from")
                                .selected_text(label)
                                .width(field_width);
                            if flat {
                                let icon = std::rc::Rc::clone(&icon);
                                combo = combo.icon(move |_, rect, _, is_open| {
                                    icon.set((rect, is_open));
                                });
                            }
                            let list = combo.show_ui(ui, |ui| {
                                for (id, name, address) in &accounts {
                                    ui.selectable_value(
                                        from,
                                        Some(id.clone()),
                                        format!("{name} <{address}>"),
                                    );
                                }
                            });
                            if flat {
                                let (place, open) = icon.get();
                                paint_from_chevron(ui, &list.response, place, open);
                            }
                        });
                    }
                    // Only reachable if the From account vanished between
                    // two frames; pick the fallback rather than showing a
                    // list with nothing selected.
                    (false, None) => {
                        *from = fallback_from.clone();
                    }
                }
            };
            if inline {
                // The head bar: the face of the account this answer goes from
                // where a field's label would be, and the list beside it. The
                // separator under it is the popup's own head treatment — its
                // heading has one — and it is what makes the row a bar rather
                // than the first field of the form.
                let scheme = theme::of(ui.ctx());
                ui.horizontal(|ui| {
                    let (slot, _) = ui.allocate_exact_size(
                        egui::vec2(COMPOSE_LABEL_WIDTH, AVATAR_DIAMETER),
                        egui::Sense::hover(),
                    );
                    if let Some(account) = sender.as_ref() {
                        let centre = slot.center();
                        let painter = ui.painter();
                        painter.circle_filled(
                            centre,
                            AVATAR_DIAMETER / 2.0,
                            sender_color(scheme, &account.username),
                        );
                        painter.text(
                            centre,
                            egui::Align2::CENTER_CENTER,
                            sender_initial(&account.name),
                            egui::FontId::proportional(AVATAR_FONT_SIZE),
                            scheme.avatar_text,
                        );
                    }
                    from_list(ui, &mut draft.from, true);
                });
                ui.separator();
            } else {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [COMPOSE_LABEL_WIDTH, COMPOSE_FIELD_HEIGHT],
                        egui::Label::new("From").halign(egui::Align::RIGHT),
                    );
                    from_list(ui, &mut draft.from, false);
                });
            }

            // The address lines. On the spot they are the reply's own head,
            // flat for the same reason the From list above them is: an
            // outline and a fill belong to a form of boxes, and the answer
            // under its mail is a letter. To ends with the chevron that folds
            // the Cc and Bcc lines out under it, so an answer that needs no
            // copies shows the one address it goes to and nothing else. The
            // popup draws all of them, Subject included, in its own boxes.
            if inline {
                let (to, fold) =
                    compose_to_row(ui, &mut draft.to, field_width, sending, draft.copies_open);
                if fold.clicked() {
                    draft.copies_open = !draft.copies_open;
                }
                if first && !body_first {
                    to.request_focus();
                }
                if draft.copies_open {
                    flat_field(ui, "Cc", &mut draft.cc);
                    flat_field(ui, "Bcc", &mut draft.bcc);
                }
            } else {
                let to = field(ui, "To", &mut draft.to, "name@example.com");
                if first {
                    to.request_focus();
                }
                field(ui, "Cc", &mut draft.cc, "name@example.com");
                field(ui, "Bcc", &mut draft.bcc, "name@example.com");
                field(ui, "Subject", &mut draft.subject, "Subject");
            }

            // The mail the answer says, and the files that would go out with
            // it. On the spot the body comes first — it is what the card is
            // for, and the mail above is what it answers — with the files
            // waiting under it; the popup keeps its form's order, files over
            // body, where the row of attachments belongs to the fields above
            // it rather than to the letter below them.
            // The body is a letter's text block on the spot — the card's
            // whole column, padding to padding — and the form's field column
            // in the popup, where everything above it is a labelled box of
            // that width.
            let body_width = if inline { width } else { field_width };
            if inline {
                ui.add_space(6.0);
                let body = compose_body(ui, &mut draft.body, body_width, sending, !body_first);
                if first && body_first {
                    body.request_focus();
                }
                ui.add_space(6.0);
                (remove, pick) =
                    compose_files_row(ui, &draft, field_width, self.picking_files, sending);
            } else {
                (remove, pick) =
                    compose_files_row(ui, &draft, field_width, self.picking_files, sending);
                ui.add_space(6.0);
                compose_body(ui, &mut draft.body, body_width, sending, !body_first);
            }

            // The footer says one of three things — the server's refusal after
            // a try, that the mail is on its way, why Send cannot be pressed —
            // and nothing at all when the draft is ready to go.
            let (to_addresses, cc_addresses, bcc_addresses) = draft.recipients();
            let blocker = send_blocker(
                sender.as_ref(),
                &to_addresses,
                &cc_addresses,
                &bcc_addresses,
            );
            let said = |ui: &mut egui::Ui| {
                if let Some(error) = &draft.error {
                    ui.label(
                        egui::RichText::new(error)
                            .small()
                            .color(theme::of(ui.ctx()).danger),
                    );
                } else if sending {
                    ui.label(egui::RichText::new("Sending…").small().weak());
                } else if let Some(reason) = &blocker {
                    ui.label(egui::RichText::new(reason.as_str()).small().weak());
                }
            };
            ui.add_space(6.0);
            if inline {
                // The card's foot has no rule over it: the card is a letter,
                // and the mail above it — not a line — is what its own end is
                // read against. What it has to say stands on a row of its own
                // over the buttons, and the two open the row at the card's
                // left edge — the side the mail above is read from — rather
                // than ending it at the right, which is where the popup's form
                // keeps its own.
                if draft.error.is_some() || sending || blocker.is_some() {
                    ui.horizontal(|ui| said(ui));
                }
                ui.horizontal(|ui| {
                    // Discard first and Send beside it: the card's own two, an
                    // icon each and room around them, Send in the accent
                    // because sending is what the card is for, and Discard an
                    // outline that turns the scheme's danger on hover — the
                    // mail it throws away is the only copy there is, and
                    // nothing is saved.
                    let drop = compose_button(
                        ui,
                        "Discard",
                        |p, rect, color| paint_delete_icon(p, rect, color, COMPOSE_BUTTON_ICON),
                        false,
                        !sending,
                    );
                    if drop.clicked() {
                        discard = true;
                    }
                    if sending {
                        drop.on_disabled_hover_text("The mail is already on its way");
                    }
                    let go = compose_button(
                        ui,
                        if sending { "Sending…" } else { "Send" },
                        paint_send_icon,
                        true,
                        !sending && blocker.is_none(),
                    );
                    if go.clicked() {
                        send = true;
                    }
                    if let Some(reason) = &blocker {
                        go.on_disabled_hover_text(reason.as_str());
                    }
                });
            } else {
                // The popup keeps the form's rule and its one-line footer:
                // the reason reads beside the two stock buttons it belongs to.
                ui.separator();
                ui.horizontal(|ui| {
                    said(ui);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let response = ui.add_enabled(
                            !sending && blocker.is_none(),
                            egui::Button::new(if sending { "Sending…" } else { "Send" }),
                        );
                        if response.clicked() {
                            send = true;
                        }
                        if let Some(reason) = &blocker {
                            response.on_disabled_hover_text(reason.as_str());
                        }
                        if ui
                            .add_enabled(!sending, egui::Button::new("Discard"))
                            .clicked()
                        {
                            discard = true;
                        }
                    });
                });
            }
        }

        // A file taken back off the mail is dropped here rather than inside the
        // drawing above: the borrow the cards hold is over, and the run must not
        // be drawn from a half-changed list.
        if let Some(index) = remove {
            draft.attachments.remove(index);
            draft.attach_error = None;
        }
        // The desktop's dialog is a job of its own: it does not come back until
        // the user has answered, so nothing here waits for it — the answer
        // arrives as an event (`Event::FilesChosen`).
        if pick {
            self.picking_files = true;
            sync::spawn(Job::PickFiles, self.ctx.clone(), self.tx.clone());
        }

        // Discarded: the draft goes with the card. There is no other copy of it
        // and nowhere it could have been saved to. A discarded card cannot be a
        // sending one — Discard is drawn disabled while a mail is in flight —
        // and the draft is put back for any other frame.
        let outcome = if send {
            ComposeOutcome::Send
        } else if discard {
            ComposeOutcome::Discard
        } else {
            ComposeOutcome::Shown
        };
        if !discard {
            self.compose = Some(draft);
        }
        outcome
    }

    /// The middle column while no search is active: the selected folder's mail.
    ///
    /// The rows are drawn straight out of the account state — mail bodies are
    /// large, so the list never copies them — and only the ones the viewport
    /// shows are laid out at all (see `virtual_rows`). A click is remembered as
    /// the mail it landed on and opened after the list has been drawn, when the
    /// borrow of the state it came from is over.
    ///
    /// Returns the height of what it drew — the rows and the list's own padding
    /// — which is the height of the column's card, or `None` when the folder has
    /// nothing to sit on (see `mail_list`).
    fn folder_messages(&mut self, ui: &mut egui::Ui) -> Option<f32> {
        self.refresh_list_rows();
        // A folder the account does not have yet — INBOX before the first
        // sync, which the navigation offers because clicking it is what starts
        // the fetch — is an empty one, and gets the same placeholder. The check
        // stands on its own because what follows holds borrows of the state it
        // reads, and the placeholder writes back to `self`.
        let listed = self
            .selected_account
            .as_deref()
            .zip(self.selected_folder.as_deref())
            .is_some_and(|(account_id, folder)| {
                self.folder_messages_of(account_id, folder)
                    .is_some_and(|messages| !messages.is_empty())
            });
        // A folder with mail in it can still have nothing to show: the column's
        // filter is what decides that (`ListRows::rows`), and an empty list is
        // the placeholder's business rather than a card with no rows on it.
        let shown = self
            .list_rows
            .as_ref()
            .is_some_and(|rows| !rows.rows.is_empty());
        if !listed || !shown {
            self.list_scroll = 0.0;
            ui.centered_and_justified(|ui| {
                let text = if !listed {
                    if self.selected_folder_synced() {
                        "This folder is empty."
                    } else {
                        "Not fetched yet — click Sync in the title bar."
                    }
                } else {
                    "No mail matches this filter."
                };
                ui.label(egui::RichText::new(text).weak().heading());
            });
            // Nothing to scroll: whatever the folder holds, this *is* the
            // bottom of the list.
            self.load_older_button(ui);
            return None;
        }
        // The rows are the list's model and the mail they name lives in the
        // account state: both are read here, and only read.
        let rows = self.list_rows.as_ref().expect("derived just above");
        let (Some(account), Some(folder)) = (
            self.selected_account.as_deref(),
            self.selected_folder.as_deref(),
        ) else {
            return None;
        };
        let messages = self
            .folder_messages_of(account, folder)
            .expect("checked just above");
        let mut clicked: Option<MailMessage> = None;
        // A conversation's chevron was clicked: whether it opens or closes is
        // applied once the borrow of the state is over.
        let mut toggled: Option<String> = None;
        // A box was clicked: the mail it stands for — one mail, or every mail of
        // a conversation — is checked once the borrow of the state is over.
        let mut checked: Option<Vec<MailKey>> = None;
        // A row's own delete button was clicked: the mail the row speaks for
        // goes once the borrow of the state is over. Its flag button, beside it,
        // is the same mail marked where it is listed.
        let mut deleted: Option<MailKey> = None;
        let mut flagged: Option<MailKey> = None;
        // A click on a row in select mode is the way out of it, and nothing
        // else: it does not open the mail under the pointer.
        let mut leave_select = false;
        // The select-all row was clicked: every mail the folder holds is
        // checked — or the selection is cleared — once the borrow is over.
        let mut select_all_clicked: Option<Vec<MailKey>> = None;
        // Select mode pins the select-all row over the top of the list — see
        // `select_all_band` — so the frame's top margin grows by its height and
        // the rows start below it: `pad_top` is what `virtual_rows` measures the
        // viewport against.
        let select_all = self.select_mode;
        let margin = egui::Margin {
            top: LIST_PADDING.top + if select_all { SELECT_ALL_HEIGHT_I8 } else { 0 },
            ..LIST_PADDING
        };
        let rows_top = LIST_PADDING.top as f32 + if select_all { SELECT_ALL_HEIGHT } else { 0.0 };
        let select_all_id = row_widget_id("select-all", account, folder, "");
        // A window narrower than the list's minimum does not crush the rows
        // further: they keep the minimum and the column scrolls sideways
        // instead — the same answer the reading pane gives a mail wider than
        // itself. The bar is the scroll area's own, so it appears only while
        // the window really is too narrow.
        let scrolls = self.list_scrolls;
        let list = if scrolls {
            egui::ScrollArea::both()
        } else {
            egui::ScrollArea::vertical()
        }
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            egui::Frame::new().inner_margin(margin).show(ui, |ui| {
                // The list's own width is its minimum; the window's is what
                // scrolls over it (see `virtual_rows`, which lays the rows
                // out at the width this leaves them).
                if scrolls {
                    ui.set_min_width(LIST_MIN_WIDTH);
                }
                // No gap between rows: the two pixels that separate two
                // mails are the rows' own edges, so the layout must not
                // add space of its own between them.
                ui.spacing_mut().item_spacing.y = 0.0;
                let count = rows.rows.len();
                virtual_rows(
                    ui,
                    viewport,
                    rows_top,
                    MESSAGE_ROW_HEIGHT,
                    count,
                    |ui, drawn| {
                        for index in drawn {
                            let first = index == 0;
                            match rows.rows[index] {
                                ListRow::Thread { thread } => {
                                    let conversation = &rows.threads[thread];
                                    let open = self.open_threads.contains(&conversation.key);
                                    let drawn =
                                        self.thread_row(ui, conversation, messages, open, first);
                                    // The row's own delete button takes the
                                    // mail the row speaks for, the box
                                    // checks every mail the conversation
                                    // holds, the chevron opens and closes
                                    // them in the list, and the row opens
                                    // the conversation in the pane — or, in
                                    // select mode, leaves it. A click on one
                                    // is not a click on another.
                                    if drawn.flag.clicked() {
                                        flagged =
                                            row_action_target(rows.rows[index], rows, messages)
                                                .map(|msg| MailKey::new(account, folder, &msg.uid));
                                    } else if drawn.delete.clicked() {
                                        deleted =
                                            row_action_target(rows.rows[index], rows, messages)
                                                .map(|msg| MailKey::new(account, folder, &msg.uid));
                                    } else if drawn.checkbox.clicked() {
                                        checked = Some(
                                            conversation
                                                .messages
                                                .iter()
                                                .map(|&index| {
                                                    MailKey::new(
                                                        account,
                                                        folder,
                                                        &messages[index].uid,
                                                    )
                                                })
                                                .collect(),
                                        );
                                    } else if drawn.chevron.is_some_and(|chevron| chevron.clicked())
                                    {
                                        toggled = Some(conversation.key.clone());
                                    } else if drawn.row.clicked() {
                                        if self.select_mode {
                                            leave_select = true;
                                        } else {
                                            clicked = Some(messages[conversation.newest].clone());
                                        }
                                    }
                                }
                                ListRow::Message { index, in_thread } => {
                                    let place = if in_thread {
                                        RowPlace::InThread
                                    } else {
                                        RowPlace::Plain
                                    };
                                    let drawn =
                                        self.message_row(ui, &messages[index], first, place);
                                    if drawn.flag.clicked() {
                                        flagged = Some(MailKey::new(
                                            account,
                                            folder,
                                            &messages[index].uid,
                                        ));
                                    } else if drawn.delete.clicked() {
                                        deleted = Some(MailKey::new(
                                            account,
                                            folder,
                                            &messages[index].uid,
                                        ));
                                    } else if drawn.checkbox.clicked() {
                                        checked = Some(vec![MailKey::new(
                                            account,
                                            folder,
                                            &messages[index].uid,
                                        )]);
                                    } else if drawn.row.clicked() {
                                        if self.select_mode {
                                            leave_select = true;
                                        } else {
                                            clicked = Some(messages[index].clone());
                                        }
                                    }
                                }
                            }
                        }
                    },
                );
            });
        });
        // The select-all row is drawn after the rows, over the top of the
        // viewport: the list scrolls under it, so the one control that is about
        // the whole list is there whether the list is at its top or its foot.
        // Its keys are the folder's mail and not the rows the viewport shows.
        if select_all {
            let band = select_all_band(list.inner_rect);
            // What the list shows rather than what the folder holds: a filter
            // that hides mail must not have it checked by a click on this row.
            let shown = rows.shown_indices();
            let state = select_all_state(self.selected_mails.len(), shown.len());
            if self.select_all_row(ui, band, state, shown.len(), select_all_id) {
                select_all_clicked = Some(
                    shown
                        .iter()
                        .map(|&index| MailKey::new(account, folder, &messages[index].uid))
                        .collect(),
                );
            }
        }
        // Scrolled away from the top, the bar floats over this list.
        self.list_scroll = list.state.offset.y;
        // The bottom of the list is the one place where "older" is what the eye
        // is on, so it is where the button belongs. A list shorter than its
        // viewport is at its bottom by definition.
        let end = (list.content_size.y - list.inner_rect.height()).max(0.0);
        let at_end = list.state.offset.y >= end - 1.0;
        // The chevron opens or closes the thread's mails in the list, and
        // nothing else: the conversation in the pane is the row's business, and
        // opening a thread does not move the reading pane. The rows this frame
        // was drawn from are the ones it had before the click, so the change is
        // asked for now — a pointer that stops moving asks for no frame of its
        // own, and the list would sit there looking as if the click was lost.
        if let Some(key) = toggled {
            if !self.open_threads.remove(&key) {
                self.open_threads.insert(key);
            }
            self.open_rev += 1;
            self.ctx.request_repaint();
        }
        // A checked row, a row's own flag or delete, the select-all row, and the
        // way out of select mode. Like the chevron, what the click made is what
        // the next frame draws, so the frame is asked for here rather than waited
        // for; the deleted row leaves when the worker says the server agreed.
        if let Some(keys) = checked {
            self.toggle_check(keys);
            self.ctx.request_repaint();
        }
        if let Some(key) = flagged {
            self.toggle_flag_on(&key);
        }
        if let Some(key) = deleted {
            self.delete_mail(&key.account, &key.folder, &key.uid);
        }
        if let Some(keys) = select_all_clicked {
            self.toggle_all(keys);
            self.ctx.request_repaint();
        }
        if leave_select {
            self.cancel_select();
            self.ctx.request_repaint();
        }
        if at_end {
            self.load_older_button(ui);
        }
        if let Some(msg) = clicked {
            self.open_message(&msg);
        }
        Some(list.content_size.y)
    }

    /// The list's model, brought up to date when anything it is derived from
    /// has moved: the folder the pane shows and its account, that folder's mail
    /// (`AccountUiState::folders_rev`), the threading switch, and the
    /// conversations the user opened.
    ///
    /// Grouping a folder is a map, a union-find and a sort over all of its
    /// mail, and the list is drawn on every frame — which is why the answer is
    /// kept rather than rebuilt (see [`ListRows`]).
    fn refresh_list_rows(&mut self) {
        let (Some(account), Some(folder)) =
            (self.selected_account.clone(), self.selected_folder.clone())
        else {
            self.list_rows = None;
            return;
        };
        let rev = self
            .states
            .get(&account)
            .map_or(0, |state| state.folders_rev);
        if let Some(rows) = &self.list_rows {
            let fresh = rows.account == account
                && rows.folder == folder
                && rows.rev == rev
                && rows.open_rev == self.open_rev
                && rows.threaded == self.threaded
                && rows.filters == self.list_filters
                && rows.sort == self.list_sort;
            if fresh {
                return;
            }
        }

        // Which conversations are open is the user's own doing — the chevron is
        // the only thing that changes it — so a folder the pane moves to opens
        // nothing: the reading pane shows the conversation where the mail is,
        // and the list stays the shape the user left it in.
        let open = self.open_threads.clone();

        // The mail is borrowed from the state and the model lives on `self`, so
        // what is derived is built from its own borrow and handed over once it
        // is owned.
        let filters = self.list_filters.clone();
        let sort = self.list_sort;
        let me = self.my_addresses();
        let derived = self.folder_messages_of(&account, &folder).map(|messages| {
            // What the column's filter lets through, in the folder's own order:
            // the conversations are trimmed to it and the flat list is drawn
            // from it, so both shapes of the list show the same mail.
            let shown: Vec<bool> = messages
                .iter()
                .map(|mail| filters.matches(mail_facts(mail), &me))
                .collect();
            let threads = threads::group(messages);
            let mut threads = if filters.is_all() {
                threads
            } else {
                threads
                    .into_iter()
                    .filter_map(|thread| thread_of_shown(thread, messages, &shown))
                    .collect()
            };
            // The column's own order, over the conversations' rows and over the
            // flat list both: what a row is sorted by is what the row shows, and
            // the order the default asks for is the folder's own, so nothing
            // moves until the menu is used.
            threads.sort_by(|one, two| {
                sort.compare(
                    thread_sort_key(one, messages),
                    thread_sort_key(two, messages),
                )
            });
            let flat = flat_order(&shown, messages, sort);
            let rows = list_rows_of(&threads, &flat, &open, self.threaded);
            (threads, rows)
        });
        self.list_rows = derived.map(|(threads, rows)| ListRows {
            account,
            folder,
            rev,
            open_rev: self.open_rev,
            threaded: self.threaded,
            filters,
            sort,
            threads,
            rows,
        });
    }

    /// The addresses the accounts are *of*: each account's username, when that
    /// is an email address — the same rule the compose applies when it says what
    /// a mail would be sent as (`crate::outgoing::is_address`). An account whose
    /// username is a bare login has no address Kimo can name, and so contributes
    /// none: it is what the list filter's "to me" is asked against, and what its
    /// menu says when there is nothing to ask.
    fn my_addresses(&self) -> Vec<String> {
        self.accounts
            .iter()
            .map(|account| account.username.trim())
            .filter(|username| crate::outgoing::is_address(username))
            .map(str::to_string)
            .collect()
    }

    /// The label group's rows and the mail the filter selects, derived when
    /// anything they depend on has moved: a label made or put on a mail
    /// (`labels_rev`), the accounts' mail (`folders_rev` — mail arriving, or a
    /// labelled mail being deleted) and the filter itself. A frame that changed
    /// nothing re-derives nothing.
    fn refresh_label_view(&mut self) {
        let mail_rev = self
            .states
            .values()
            .fold(0_u64, |sum, state| sum.wrapping_add(state.folders_rev));
        let key = (
            self.labels_rev,
            mail_rev,
            self.label_filter.clone(),
            self.list_sort,
        );
        if self
            .label_view
            .as_ref()
            .is_some_and(|view| view.key.as_ref() == Some(&key))
        {
            return;
        }
        // The counts are about every label; the matches are only asked for while
        // a filter is on. An empty filter matches everything, which is exactly
        // what must not be collected.
        let filtered = !self.label_filter.is_empty();
        let mut counts = vec![0_usize; self.labels.len()];
        let mut matches = Vec::new();
        for (account_id, state) in &self.states {
            for folder in &state.mailbox.folders {
                for message in &folder.messages {
                    if message.labels.is_empty() {
                        continue;
                    }
                    for (index, name) in self.labels.iter().enumerate() {
                        if message.labels.contains(name) {
                            counts[index] += 1;
                        }
                    }
                    if !filtered
                        || !self
                            .label_filter
                            .iter()
                            .all(|name| message.labels.contains(name))
                    {
                        continue;
                    }
                    matches.push(LabelledMail {
                        account_id: account_id.clone(),
                        folder: folder.name.clone(),
                        uid: message.uid.clone(),
                        from: message.from.clone(),
                        to: message.to.clone(),
                        subject: message.subject.clone(),
                        date: message.date.clone(),
                        timestamp: message.timestamp,
                        seen: message.seen,
                        flagged: message.flagged,
                        attachments: message.attachments.len(),
                    });
                }
            }
        }
        // The column's own order — the sort cell's, which is newest first by
        // date until the menu is used — and a total order besides: the filter
        // spans every folder and account, so two mails that compare equal would
        // otherwise come out in whatever order the accounts were walked in.
        let sort = self.list_sort;
        matches.sort_by(|a, b| {
            sort.compare(labelled_sort_key(a), labelled_sort_key(b))
                .then_with(|| a.account_id.cmp(&b.account_id))
                .then_with(|| a.folder.cmp(&b.folder))
                .then_with(|| a.uid.cmp(&b.uid))
        });
        let rows = self
            .labels
            .iter()
            .zip(&counts)
            .map(|(name, &count)| LabelCount {
                name: name.clone(),
                count,
                selected: self.label_filter.iter().any(|label| label == name),
            })
            .collect();
        self.label_view = Some(LabelView {
            key: Some(key),
            rows,
            matches,
        });
    }

    /// Look at the mail one label holds, or stop looking at it: what a click on a
    /// label in the navigation column does. One label narrows the list to the
    /// mail that carries it, a second narrows it further — the filter is an AND,
    /// so the list holds the mail that carries both — and a click on a selected
    /// label takes it back out. The last one out leaves the folders' own list.
    ///
    /// The reading pane is deliberately left alone: the mail it shows is still
    /// the mail it showed, and the filled folder row still says where that mail
    /// is filed.
    fn toggle_label(&mut self, name: &str) {
        match self.label_filter.iter().position(|label| label == name) {
            Some(index) => {
                self.label_filter.remove(index);
            }
            None => {
                self.label_filter.push(name.to_string());
                self.label_filter.sort();
            }
        }
        // Another list than the one the checks were made on.
        self.cancel_select();
        // A query and a label filter are two answers to what the middle column
        // shows, and the label is the one that was just asked for.
        self.end_search();
        self.ctx.request_repaint();
    }

    /// End an active search, leaving the column to the folders — or to a label
    /// filter that is about to take it. The popup's own Clear is the same thing
    /// with its boxes in front of the user; this is what a label click does to a
    /// query it replaces.
    fn end_search(&mut self) {
        if !self.search.is_active() {
            return;
        }
        self.search = SearchQuery::default();
        self.search_pending = None;
        self.spawn_search();
    }

    /// The key of a mail the reading pane is showing: the account and folder the
    /// pane reads from and the mail's own uid, which is what the label tables
    /// name a message by.
    fn pane_mail_key(&self, uid: &str) -> Option<MailKey> {
        let account = self.selected_account.as_deref()?;
        let folder = self.selected_folder.as_deref()?;
        Some(MailKey::new(account, folder, uid))
    }

    /// The key of the mail the title bar's mail actions are about, if the pane
    /// shows one — the pane's own lookup, so a uid it no longer shows is no
    /// mail.
    fn selected_mail_key(&self) -> Option<MailKey> {
        if !self.has_selected_message() {
            return None;
        }
        self.pane_mail_key(self.selected_message.as_deref()?)
    }

    /// The labels one mail carries, by name, read from the app's copy of the
    /// mail — which is what the popup's checklist is drawn from and what the
    /// card's chips say.
    fn mail_labels(&self, key: &MailKey) -> Vec<String> {
        self.states
            .get(&key.account)
            .and_then(|state| {
                state
                    .mailbox
                    .folders
                    .iter()
                    .find(|folder| folder.name == key.folder)
            })
            .and_then(|folder| folder.messages.iter().find(|mail| mail.uid == key.uid))
            .map(|mail| mail.labels.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Put one label on a mail or take it off: what every click in the label
    /// popup does.
    ///
    /// The app's own copy moves first, so the frame that follows draws it, and a
    /// worker makes it durable (`Job::SetLabels`) — with the whole set, because
    /// that is what the label tables hold. A name that is not a label yet is made
    /// one by this click and by nothing else, and a name that differs only in
    /// case from one that exists is that label: the spelling the user made first
    /// is the one kept.
    fn set_label_on_mail(&mut self, key: &MailKey, name: &str, on: bool) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let label = self
            .labels
            .iter()
            .find(|existing| existing.eq_ignore_ascii_case(name))
            .cloned()
            .unwrap_or_else(|| name.to_string());
        let updated: Option<Vec<String>> = self
            .states
            .get_mut(&key.account)
            .and_then(|state| {
                state
                    .mailbox
                    .folders
                    .iter_mut()
                    .find(|folder| folder.name == key.folder)
            })
            .and_then(|folder| folder.messages.iter_mut().find(|mail| mail.uid == key.uid))
            .map(|mail| {
                if on {
                    mail.labels.insert(label.clone());
                } else {
                    mail.labels.remove(&label);
                }
                mail.labels.iter().cloned().collect()
            });
        // The mail has left the state (deleted, or a folder opened in its
        // place): there is nothing to label.
        let Some(labels) = updated else {
            return;
        };
        if on && !self.labels.contains(&label) {
            self.labels.push(label);
            // The order the column draws them in, which is the database's own
            // (`db::load_labels`).
            self.labels.sort_by_key(|name| name.to_lowercase());
        }
        self.labels_rev = self.labels_rev.wrapping_add(1);
        sync::spawn(
            Job::SetLabels {
                account_id: key.account.clone(),
                folder: key.folder.clone(),
                uid: key.uid.clone(),
                labels,
            },
            self.ctx.clone(),
            self.tx.clone(),
        );
        self.ctx.request_repaint();
    }

    /// Put the flag on the app's own copy of a mail, or take it off: what the
    /// list's warning tint and the pane card's fill are read from. The database
    /// is written by the job behind this, not here (`crate::sync`'s
    /// `set_flag`), so a write that failed leaves the two agreeing again the
    /// next time the folder is loaded.
    ///
    /// A search's own hits are flipped too: they are a copy of the mail rather
    /// than the mail itself, and the query that built them is not re-run by a
    /// flag. A conversation's row takes the flag of any mail of its own
    /// (`Thread::flagged`), and the thread and the label view are derived from
    /// the account's revision, which is what `folders_changed` moves.
    fn set_flag_on_mail(&mut self, key: &MailKey, flagged: bool) {
        let Some(state) = self.states.get_mut(&key.account) else {
            return;
        };
        let Some(mail) = state
            .mailbox
            .folders
            .iter_mut()
            .find(|folder| folder.name == key.folder)
            .and_then(|folder| folder.messages.iter_mut().find(|mail| mail.uid == key.uid))
        else {
            return;
        };
        if mail.flagged == flagged {
            return;
        }
        mail.flagged = flagged;
        state.folders_changed();
        for hit in &mut self.search_hits {
            if hit.account_id == key.account && hit.folder == key.folder && hit.uid == key.uid {
                hit.flagged = flagged;
            }
        }
    }

    /// The label popup: the mail's labels as a checklist, and a box that makes a
    /// new one by putting it on the mail — which is the only way a label comes
    /// into being. Anchored to the cell that opened it (`Popup::menu`), and
    /// closed by a click outside it rather than by a click on a label: picking
    /// several is what the popup is for.
    fn label_popup(&mut self, response: &egui::Response, key: &MailKey) {
        let attached = self.mail_labels(key);
        let mut toggled: Option<(String, bool)> = None;
        let mut created = false;
        egui::Popup::menu(response)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .width(LABEL_POPUP_WIDTH)
            .show(|ui| {
                ui.set_min_width(LABEL_POPUP_WIDTH);
                if self.labels.is_empty() {
                    ui.label(
                        egui::RichText::new("No labels yet — name one below.")
                            .small()
                            .weak(),
                    );
                }
                egui::ScrollArea::vertical()
                    .max_height(LABEL_POPUP_MAX_HEIGHT)
                    .show(ui, |ui| {
                        for name in &self.labels {
                            let mut checked = attached.contains(name);
                            if ui.checkbox(&mut checked, name).changed() {
                                toggled = Some((name.clone(), checked));
                            }
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    let field = ui.add(Field::new(
                        egui::TextEdit::singleline(&mut self.label_draft)
                            .hint_text("New label…")
                            .desired_width(LABEL_POPUP_FIELD_WIDTH),
                    ));
                    let enter = entered(&field, ui);
                    if (ui.button("Create").clicked() || enter)
                        && !self.label_draft.trim().is_empty()
                    {
                        created = true;
                    }
                });
            });
        if let Some((name, on)) = toggled {
            self.set_label_on_mail(key, &name, on);
        } else if created {
            let name = std::mem::take(&mut self.label_draft);
            self.set_label_on_mail(key, name.trim(), true);
        }
    }

    /// The filter menu: the four views of the list as checkboxes, under the "All"
    /// row that is the way out of every one of them. Ticking a box narrows the
    /// list, unticking it widens the list again, and the menu stays open across
    /// ticks — the views are optional and they are an AND, so picking several is
    /// the point (like the label popup, and unlike a menu of one choice).
    ///
    /// Anchored to the cell that opened it (`Popup::menu`). "All" is not one of
    /// the views: it is drawn ticked exactly while none is, and ticking it clears
    /// the rest (`ListFilters::clear`). "To me" is a question about an address the
    /// accounts have, so a session whose accounts are all bare logins has nothing
    /// to ask — the box says so rather than filtering to nothing.
    fn filter_popup(&mut self, response: &egui::Response) {
        let me = self.my_addresses();
        // The boxes are drawn from a copy and written back once the menu has
        // closed: the views are an AND, so ticking one never takes another off —
        // except "All", which is the empty set and clears them.
        let mut filters = self.list_filters.clone();
        let mut changed = false;
        egui::Popup::menu(response)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .width(LIST_MENU_WIDTH)
            .show(|ui| {
                ui.set_min_width(LIST_MENU_WIDTH);
                ui.spacing_mut().item_spacing.y = FILTER_ROW_GAP;
                // "All" is not one of the views: it is the absence of them, drawn
                // ticked exactly while none is, and ticking it clears the rest.
                if menu_row(ui, MenuIcon::All, "All", filters.is_all(), true)
                    .on_hover_text("Every mail in the list")
                    .clicked()
                {
                    filters.clear();
                    changed = true;
                }
                for filter in ListFilter::ALL {
                    // "To me" is a question about an address the accounts have:
                    // a session whose accounts are all bare logins has nothing to
                    // ask, and the row says so rather than filtering to nothing.
                    let asked = filter != ListFilter::ToMe || !me.is_empty();
                    let row = menu_row(
                        ui,
                        filter.icon(),
                        filter.label(),
                        filters.contains(filter),
                        asked,
                    );
                    let row = if asked {
                        row.on_hover_text(filter.note())
                    } else {
                        row.on_disabled_hover_text(
                            "No account of yours has an email address as its username",
                        )
                    };
                    if row.clicked() {
                        filters.toggle(filter);
                        changed = true;
                    }
                }
            });
        if changed && filters != self.list_filters {
            self.set_list_filters(filters);
        }
    }

    /// Show a different set of views of the message list: an empty set is all
    /// mail, and each ticked view narrows it further — the AND the menu's boxes
    /// are.
    ///
    /// The derived rows carry the set they were built with
    /// (`ListRows::filters`), so the next frame derives them again; the frame is
    /// asked for here because a tick on a menu box is the last thing that happens
    /// in this one.
    fn set_list_filters(&mut self, filters: ListFilters) {
        if self.list_filters == filters {
            return;
        }
        self.list_filters = filters;
        self.ctx.request_repaint();
    }

    /// The sort menu: the *order* group — newest on top, or oldest — and the
    /// *by* group — date, sender, subject — each an independent choice of one,
    /// drawn as a row per choice with the one in force ticked at its end.
    ///
    /// Anchored to the cell that opened it (`Popup::menu`) and left open across
    /// picks, like the filter's menu and for the same reason: changing one's
    /// mind about the order and about the key is two clicks, not two visits.
    fn sort_popup(&mut self, response: &egui::Response) {
        let mut sort = self.list_sort;
        let mut changed = false;
        egui::Popup::menu(response)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .width(LIST_MENU_WIDTH)
            .show(|ui| {
                ui.set_min_width(LIST_MENU_WIDTH);
                ui.spacing_mut().item_spacing.y = FILTER_ROW_GAP;
                menu_heading(ui, "Sort order");
                for order in SortOrder::ALL {
                    let row = menu_row(ui, order.icon(), order.label(), sort.order == order, true);
                    if row.clicked() {
                        sort.order = order;
                        changed = true;
                    }
                }
                ui.separator();
                menu_heading(ui, "Sort by");
                for by in SortBy::ALL {
                    let row = menu_row(ui, by.icon(), by.label(), sort.by == by, true);
                    if row.clicked() {
                        sort.by = by;
                        changed = true;
                    }
                }
            });
        if changed && sort != self.list_sort {
            self.set_list_sort(sort);
        }
    }

    /// Order the message list the way the sort menu asked: the other half of what
    /// the column's view is, beside the filter.
    ///
    /// The derived rows carry the sort they were built with (`ListRows::sort`), so
    /// the next frame derives them again; the frame is asked for here because a
    /// click on a menu row is the last thing that happens in this one.
    fn set_list_sort(&mut self, sort: ListSort) {
        if self.list_sort == sort {
            return;
        }
        self.list_sort = sort;
        self.ctx.request_repaint();
    }

    /// The folder the list is showing, as `(account, folder)`: what a folder    /// The folder the list is showing, as `(account, folder)`: what a folder
    /// row's selection key and its checkbox id are built from. The rows are only
    /// drawn while a folder is open — the search results have a row of their
    /// own, `search_row`, which names the folder each hit lives in — so the two
    /// empty fallbacks describe a list that cannot be drawn anyway.
    fn list_folder(&self) -> (&str, &str) {
        (
            self.selected_account.as_deref().unwrap_or_default(),
            self.selected_folder.as_deref().unwrap_or_default(),
        )
    }

    /// What a folder row's checkbox draws: `None` in view mode — where the
    /// avatar stands and the pointer alone brings an empty box over the row it
    /// is on — and otherwise the state of the mail the row stands for. `uids`
    /// is that mail, or every mail of the conversation the row speaks for.
    fn folder_checkbox(&self, uids: &[&str]) -> Option<CheckState> {
        if !self.select_mode {
            return None;
        }
        let (account, folder) = self.list_folder();
        let keys: Vec<MailKey> = uids
            .iter()
            .map(|uid| MailKey::new(account, folder, uid))
            .collect();
        Some(self.check_state(&keys))
    }

    /// The same for one search hit, which names the folder it lives in — one
    /// hit is one mail, so its box has no state between off and on.
    fn hit_checkbox(&self, hit: &SearchHit) -> Option<CheckState> {
        if !self.select_mode {
            return None;
        }
        let key = MailKey::new(&hit.account_id, &hit.folder, &hit.uid);
        Some(if self.selected_mails.contains(&key) {
            CheckState::On
        } else {
            CheckState::Off
        })
    }

    /// The same for one row of a label filter, whose mail is not necessarily
    /// the open folder's: one mail is one row, so its box has no state between
    /// off and on.
    fn labelled_checkbox(&self, mail: &LabelledMail) -> Option<CheckState> {
        if !self.select_mode {
            return None;
        }
        let key = MailKey::new(&mail.account_id, &mail.folder, &mail.uid);
        Some(if self.selected_mails.contains(&key) {
            CheckState::On
        } else {
            CheckState::Off
        })
    }

    /// What a box says about the mails it stands for: nothing checked is off,
    /// everything checked is on, and a conversation with only some of its mails
    /// checked — which is what opening it and unchecking one mail reaches — is
    /// in between. An empty run is nothing checked, which is what a row that
    /// stands for no mail says.
    fn check_state(&self, keys: &[MailKey]) -> CheckState {
        let checked = keys
            .iter()
            .filter(|key| self.selected_mails.contains(*key))
            .count();
        match checked {
            0 => CheckState::Off,
            n if n == keys.len() => CheckState::On,
            _ => CheckState::Partial,
        }
    }

    /// A checkbox was clicked: that is what turns select mode on, and the mails
    /// the row stands for go into the selection — or out of it when they were
    /// all in it, which is how a checked conversation is unchecked.
    fn toggle_check(&mut self, keys: Vec<MailKey>) {
        if keys.is_empty() {
            return;
        }
        self.select_mode = true;
        let all = keys.iter().all(|key| self.selected_mails.contains(key));
        for key in keys {
            if all {
                self.selected_mails.remove(&key);
            } else {
                self.selected_mails.insert(key);
            }
        }
    }

    /// Leave select mode and drop what was checked. The column is a list of
    /// something — a folder, a query — and a selection is a selection *of* it:
    /// whatever moves the column to another list ends the mode rather than
    /// leaving checks behind for rows that are not on screen any more.
    fn cancel_select(&mut self) {
        self.select_mode = false;
        self.selected_mails.clear();
    }

    /// Check every mail of the list, or clear the selection when it is all in:
    /// the select-all row's click. Clearing is the one answer that can never
    /// leave a key behind for a row that is not on screen — and the mode stays
    /// on, exactly as it does when the last box is unchecked by hand.
    fn toggle_all(&mut self, keys: Vec<MailKey>) {
        if keys.is_empty() {
            return;
        }
        if keys.iter().all(|key| self.selected_mails.contains(key)) {
            self.selected_mails.clear();
        } else {
            self.selected_mails.extend(keys);
        }
    }

    /// The list's select-all row: the checkbox in the rows' own column, the
    /// label that says what a click would do, and the hairline under it. It is
    /// the header of the checkbox column while select mode is on, and nothing
    /// outside the mode — not the list header the column deliberately has none
    /// of.
    ///
    /// The whole band is the click target, unlike a mail row's box: there is
    /// nothing else a click on it could mean, so a slightly mis-aimed click on
    /// the label must not be what leaves the mode. Returns whether the row was
    /// clicked; the caller checks or clears the list once it has stopped
    /// borrowing it.
    fn select_all_row(
        &self,
        ui: &mut egui::Ui,
        band: egui::Rect,
        state: CheckState,
        listed: usize,
        id: egui::Id,
    ) -> bool {
        let scheme = theme::of(ui.ctx());
        let column = self.list_column;
        let response = ui.interact(band, id, egui::Sense::click());
        let hovered = response.hovered();
        let painter = ui.painter();
        let fill = egui::Rect::from_x_y_ranges(column.x_range(), band.y_range());
        // The rows scroll under the band — it is the one control of the column
        // that does not scroll away — so it is opaque: the surface it sits on is
        // the list's own, filled edge to edge like a row.
        painter
            .with_clip_rect(column)
            .rect_filled(fill, 0.0, scheme.content);
        // The same whole-column hover fill a row draws, and the pointer a
        // control takes: the band is a row of the list, not a label on it.
        if hovered {
            painter
                .with_clip_rect(column)
                .rect_filled(fill, 0.0, scheme.hover);
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        // The box sits where a mail row's does — the plain-row slot, since the
        // band is not a mail of an open conversation — so the column of boxes
        // runs through it. The band spans the whole column, so the slot is
        // taken from the column and the rows' own margin, not from the band's
        // left edge.
        let centre = egui::pos2(
            column.left() + LIST_MARGIN as f32 + 8.0 + THREAD_INDENT + AVATAR_DIAMETER / 2.0,
            band.center().y,
        );
        paint_checkbox(
            painter,
            egui::Rect::from_center_size(centre, egui::Vec2::splat(CHECKBOX_SIZE)),
            state,
            hovered,
            scheme,
        );
        // The label takes the rows' own text left, so it reads as a line of the
        // list rather than as something floating beside it.
        let text_left = centre.x + AVATAR_DIAMETER / 2.0 + AVATAR_TEXT_GAP;
        let label = select_all_label(state);
        let galley = truncate_galley(
            ui.ctx(),
            label,
            ROW_PREVIEW_FONT_SIZE,
            scheme.text_weak,
            (band.right() - 10.0 - text_left).max(40.0),
        );
        painter.galley(
            egui::pos2(text_left, band.center().y - galley.size().y * 0.5),
            galley,
            scheme.text_weak,
        );
        // The hairline that separates it from the first mail: the rows draw
        // their own edges, and the band's bottom one is why the first mail still
        // draws no top edge of its own.
        painter.with_clip_rect(column).line_segment(
            [
                egui::pos2(column.left(), band.bottom() - 0.5),
                egui::pos2(column.right(), band.bottom() - 0.5),
            ],
            egui::Stroke::new(1.0_f32, scheme.row_edge),
        );
        response
            .on_hover_text(select_all_tooltip(state, listed))
            .clicked()
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
    ///
    /// Returns the height of the hits it drew, which is the height of the
    /// column's card, or `None` when there is nothing on it (see `mail_list`).
    fn search_results(&mut self, ui: &mut egui::Ui) -> Option<f32> {
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
            return None;
        }
        // The column's own filter over the hits, beside the query's: the popup's
        // "Only" boxes narrow the query in the database, this narrows the view
        // of what came back, and the two together are an AND.
        let me = self.my_addresses();
        let shown: Vec<usize> = self
            .search_hits
            .iter()
            .enumerate()
            .filter(|(_, hit)| self.list_filters.matches(hit_facts(hit), &me))
            .map(|(index, _)| index)
            .collect();
        if shown.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new("No hit matches this filter.")
                        .weak()
                        .heading(),
                );
            });
            self.list_scroll = 0.0;
            return None;
        }
        // A hit is what a click landed on, opened once the list has been drawn:
        // `open_hit` walks the account state, which the rows are borrowing. A
        // click on a hit's box checks it instead (and a click on the row itself
        // leaves select mode), and a hit names its own folder, so the key is
        // built from the hit and not from the column.
        let mut clicked: Option<SearchHit> = None;
        let mut checked: Option<Vec<MailKey>> = None;
        // A hit's own delete button was clicked: the hit goes — and its flag
        // button beside it marks the mail where the hit lives.
        let mut deleted: Option<MailKey> = None;
        let mut flagged: Option<MailKey> = None;
        let mut leave_select = false;
        // The select-all row was clicked: every hit is checked — or the
        // selection is cleared — once the borrow of the hits is over.
        let mut select_all_clicked: Option<Vec<MailKey>> = None;
        // The same band the folder's list draws, about the hits instead: one
        // hit is one mail, and each names the folder it lives in.
        let select_all = self.select_mode;
        let margin = egui::Margin {
            top: LIST_PADDING.top + if select_all { SELECT_ALL_HEIGHT_I8 } else { 0 },
            ..LIST_PADDING
        };
        let rows_top = LIST_PADDING.top as f32 + if select_all { SELECT_ALL_HEIGHT } else { 0.0 };
        let select_all_id = row_widget_id("select-all", "search", "", "");
        let hits = &self.search_hits;
        let list = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_viewport(ui, |ui, viewport| {
                egui::Frame::new().inner_margin(margin).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    virtual_rows(
                        ui,
                        viewport,
                        rows_top,
                        MESSAGE_ROW_HEIGHT,
                        shown.len(),
                        |ui, rows| {
                            for index in rows {
                                let hit = &hits[shown[index]];
                                let drawn = self.search_row(ui, hit, index == 0);
                                if drawn.flag.clicked() {
                                    flagged =
                                        Some(MailKey::new(&hit.account_id, &hit.folder, &hit.uid));
                                } else if drawn.delete.clicked() {
                                    deleted =
                                        Some(MailKey::new(&hit.account_id, &hit.folder, &hit.uid));
                                } else if drawn.checkbox.clicked() {
                                    checked = Some(vec![MailKey::new(
                                        &hit.account_id,
                                        &hit.folder,
                                        &hit.uid,
                                    )]);
                                } else if drawn.row.clicked() {
                                    if self.select_mode {
                                        leave_select = true;
                                    } else {
                                        clicked = Some(hit.clone());
                                    }
                                }
                            }
                        },
                    );
                });
            });
        // The same pinned select-all row the folder's list draws, about the
        // hits instead: one hit is one mail, and each names the folder it lives
        // in.
        if select_all {
            let band = select_all_band(list.inner_rect);
            // The hits the filter shows, not every hit the query returned.
            let state = select_all_state(self.selected_mails.len(), shown.len());
            if self.select_all_row(ui, band, state, shown.len(), select_all_id) {
                select_all_clicked = Some(
                    shown
                        .iter()
                        .map(|&index| {
                            let hit = &hits[index];
                            MailKey::new(&hit.account_id, &hit.folder, &hit.uid)
                        })
                        .collect(),
                );
            }
        }
        // A scrolled result list floats the bar exactly like the folder's does.
        self.list_scroll = list.state.offset.y;
        if let Some(keys) = checked {
            self.toggle_check(keys);
            self.ctx.request_repaint();
        }
        if let Some(key) = flagged {
            self.toggle_flag_on(&key);
        }
        if let Some(key) = deleted {
            self.delete_mail(&key.account, &key.folder, &key.uid);
        }
        if let Some(keys) = select_all_clicked {
            self.toggle_all(keys);
            self.ctx.request_repaint();
        }
        if leave_select {
            self.cancel_select();
            self.ctx.request_repaint();
        }
        if let Some(hit) = clicked {
            self.open_hit(&hit);
        }
        Some(list.content_size.y)
    }

    /// The middle column while a label filter is on: one card per mail that
    /// carries every selected label, newest first.
    ///
    /// A label filter is a lens over the whole mailbox — the mail of every
    /// folder and account at once — so a row says where its mail lives the way a
    /// search hit's does, and the list is flat rather than threaded, for the
    /// same reason the hits are: where a mail sits among its own folder's
    /// conversations is not what was asked for.
    ///
    /// Returns the height of the rows it drew, which is the height of the
    /// column's card, or `None` when there is nothing on it (see `mail_list`).
    fn label_results(&mut self, ui: &mut egui::Ui) -> Option<f32> {
        self.refresh_label_view();
        // A row was clicked: the mail it stands for is opened once the rows have
        // stopped borrowing the view they came from — the same shape the folder
        // list and the hits use. A box checks the row's mail instead (and a
        // click on the row itself leaves select mode), and the row's own delete
        // button takes it.
        let mut clicked: Option<LabelledMail> = None;
        let mut checked: Option<Vec<MailKey>> = None;
        let mut deleted: Option<MailKey> = None;
        let mut flagged: Option<MailKey> = None;
        let mut leave_select = false;
        let mut select_all_clicked: Option<Vec<MailKey>> = None;
        // The same pinned band the folder's list draws, about this list: one
        // mail is one row here, and every row names the folder it lives in.
        let select_all = self.select_mode;
        let margin = egui::Margin {
            top: LIST_PADDING.top + if select_all { SELECT_ALL_HEIGHT_I8 } else { 0 },
            ..LIST_PADDING
        };
        let rows_top = LIST_PADDING.top as f32 + if select_all { SELECT_ALL_HEIGHT } else { 0.0 };
        let select_all_id = row_widget_id("select-all", "labels", "", "");
        let Some(view) = &self.label_view else {
            return None;
        };
        if view.matches.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new("No mail carries all of these labels.")
                        .weak()
                        .heading(),
                );
            });
            // Nothing to scroll: whatever the labels hold, this *is* the bottom
            // of the list, and the bar is docked over it.
            self.list_scroll = 0.0;
            return None;
        }
        let matches = &view.matches;
        // The column's own filter over the label's mail: the label says what the
        // mail carries, the filter says which of it the column shows.
        let me = self.my_addresses();
        let shown: Vec<usize> = matches
            .iter()
            .enumerate()
            .filter(|(_, mail)| self.list_filters.matches(labelled_facts(mail), &me))
            .map(|(index, _)| index)
            .collect();
        if shown.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new("No mail matches this filter.")
                        .weak()
                        .heading(),
                );
            });
            self.list_scroll = 0.0;
            return None;
        }
        let list = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_viewport(ui, |ui, viewport| {
                egui::Frame::new().inner_margin(margin).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    virtual_rows(
                        ui,
                        viewport,
                        rows_top,
                        MESSAGE_ROW_HEIGHT,
                        shown.len(),
                        |ui, rows| {
                            for index in rows {
                                let mail = &matches[shown[index]];
                                let drawn = self.labelled_row(ui, mail, index == 0);
                                if drawn.flag.clicked() {
                                    flagged = Some(MailKey::new(
                                        &mail.account_id,
                                        &mail.folder,
                                        &mail.uid,
                                    ));
                                } else if drawn.delete.clicked() {
                                    deleted = Some(MailKey::new(
                                        &mail.account_id,
                                        &mail.folder,
                                        &mail.uid,
                                    ));
                                } else if drawn.checkbox.clicked() {
                                    checked = Some(vec![MailKey::new(
                                        &mail.account_id,
                                        &mail.folder,
                                        &mail.uid,
                                    )]);
                                } else if drawn.row.clicked() {
                                    if self.select_mode {
                                        leave_select = true;
                                    } else {
                                        clicked = Some(mail.clone());
                                    }
                                }
                            }
                        },
                    );
                });
            });
        if select_all {
            let band = select_all_band(list.inner_rect);
            // The mail the filter shows, not every mail the labels selected.
            let state = select_all_state(self.selected_mails.len(), shown.len());
            if self.select_all_row(ui, band, state, shown.len(), select_all_id) {
                select_all_clicked = Some(
                    shown
                        .iter()
                        .map(|&index| {
                            let mail = &matches[index];
                            MailKey::new(&mail.account_id, &mail.folder, &mail.uid)
                        })
                        .collect(),
                );
            }
        }
        // A scrolled filter floats the bar exactly like the folder's list does.
        self.list_scroll = list.state.offset.y;
        if let Some(keys) = checked {
            self.toggle_check(keys);
            self.ctx.request_repaint();
        }
        if let Some(key) = flagged {
            self.toggle_flag_on(&key);
        }
        if let Some(key) = deleted {
            self.delete_mail(&key.account, &key.folder, &key.uid);
        }
        if let Some(keys) = select_all_clicked {
            self.toggle_all(keys);
            self.ctx.request_repaint();
        }
        if leave_select {
            self.cancel_select();
            self.ctx.request_repaint();
        }
        // Opening a mail of the filter does not end the filter: the list is the
        // lens the user is reading the mailbox through, and the pane is what
        // they are reading — the same division a search's hits have.
        if let Some(mail) = clicked {
            self.open_mail_at(&mail.account_id, &mail.folder, &mail.uid);
        }
        Some(list.content_size.y)
    }

    /// One row of a label filter: the card a message row is, with where the mail
    /// lives as its third line. The filter spans folders and accounts, so where
    /// a mail is filed is the one thing about it the row has to add — the rest
    /// is the mail's own.
    fn labelled_row(&self, ui: &mut egui::Ui, mail: &LabelledMail, first: bool) -> RowResponse {
        let checkbox = self.labelled_checkbox(mail);
        let selected = (self.selected_account.as_deref() == Some(mail.account_id.as_str())
            && self.selected_folder.as_deref() == Some(mail.folder.as_str())
            && self.selected_message.as_deref() == Some(mail.uid.as_str()))
            || checkbox == Some(CheckState::On);
        let account = self
            .accounts
            .iter()
            .find(|account| account.id == mail.account_id)
            .map(|account| account.name.as_str())
            .unwrap_or(mail.account_id.as_str());
        let folder = crate::protocol::imap::decode_mailbox_name(&mail.folder);
        let third = format!("{account} · {folder}");
        let card = CardText {
            from: &mail.from,
            title: &mail.subject,
            date: &date_label(mail.timestamp, &mail.date),
            third: &third,
            unread: !mail.seen,
            selected,
            flagged: mail.flagged,
            has_attachments: mail.attachments > 0,
            place: RowPlace::Plain,
            checkbox,
            // Named by where the mail lives, like a hit's: two accounts' uids
            // can be the same string.
            checkbox_id: row_widget_id("mail-checkbox", &mail.account_id, &mail.folder, &mail.uid),
            delete_id: row_widget_id("mail-delete", &mail.account_id, &mail.folder, &mail.uid),
            flag_id: row_widget_id("mail-flag", &mail.account_id, &mail.folder, &mail.uid),
        };
        self.message_card(ui, &card, first)
    }

    /// Ask for what the search popup is showing, once the query has held still;
    /// `force` is Enter (or Search/Clear), which skips the wait.
    fn pump_search(&mut self, force: bool) {
        if force {
            self.search_pending = None;
            // Enter (or Search/Clear) applies the query now. A *different*
            // query is a different list of hits, and a selection is a selection
            // of the list it was made on: the checks go with it. Re-running the
            // query already applied is not a change — the list is the same one.
            if self.search_applied.as_ref() != Some(&self.search) {
                self.cancel_select();
            }
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
                    // An emptied bar goes back to the folder view at once, and
                    // the column is another list than the hits were.
                    self.search_pending = None;
                    self.cancel_select();
                    self.spawn_search();
                }
            }
            SearchWait::Run => {
                self.search_pending = None;
                // The query moved: another set of hits, and nothing checked on
                // the one before it is on this one.
                self.cancel_select();
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

    /// Open a search result: where the hit lives is all the row knows, and that
    /// is all this needs.
    fn open_hit(&mut self, hit: &SearchHit) {
        self.open_mail_at(&hit.account_id, &hit.folder, &hit.uid);
    }

    /// Open one mail by where it lives: switch to the account and folder it is
    /// filed in, then open it the way a click in the list would. What a search
    /// hit's row and a label filter's row both do — neither list is a folder's,
    /// so neither row can be opened by the column's own selection.
    fn open_mail_at(&mut self, account_id: &str, folder: &str, uid: &str) {
        let Some(protocol) = self
            .accounts
            .iter()
            .find(|account| account.id == account_id)
            .map(|account| account.protocol)
        else {
            return;
        };
        self.select_folder(account_id.to_string(), folder.to_string(), protocol);
        // The whole mailbox is in memory, so the mail is normally right there.
        // If it is not (the account was edited under us), the pane stays empty
        // instead of opening somebody else's mail.
        let message = self
            .states
            .get(account_id)
            .and_then(|state| state.mailbox.folders.iter().find(|f| f.name == folder))
            .and_then(|folder| folder.messages.iter().find(|m| m.uid == uid))
            .cloned();
        if let Some(message) = message {
            self.open_message(&message);
        }
    }

    /// One message card in the folder list. `first` is the list's own first
    /// card, which is the one that draws no line above itself, and `place` is
    /// where it sits: on its own, or under the conversation it belongs to. The
    /// row only draws itself — the list it belongs to opens whatever was
    /// clicked, once it has stopped borrowing the state this was read from.
    fn message_row(
        &self,
        ui: &mut egui::Ui,
        msg: &MailMessage,
        first: bool,
        place: RowPlace<'_>,
    ) -> RowResponse {
        // The whole row is filled for what is open in the pane and for what is
        // checked in select mode alike: the box says which mail is which, and
        // the fill says how much of the list a Delete is about.
        let checkbox = self.folder_checkbox(&[&msg.uid]);
        let selected = self.selected_message.as_deref() == Some(msg.uid.as_str())
            || checkbox == Some(CheckState::On);
        let preview = if msg.preview.is_empty() {
            " ".to_string()
        } else {
            msg.preview.clone()
        };
        let (account, folder) = self.list_folder();
        let card = CardText {
            from: &msg.from,
            title: &msg.subject,
            date: &date_label(msg.timestamp, &msg.date),
            third: &preview,
            unread: !msg.seen,
            selected,
            flagged: msg.flagged,
            has_attachments: !msg.attachments.is_empty(),
            place,
            checkbox,
            checkbox_id: row_widget_id("mail-checkbox", account, folder, &msg.uid),
            delete_id: row_widget_id("mail-delete", account, folder, &msg.uid),
            flag_id: row_widget_id("mail-flag", account, folder, &msg.uid),
        };
        self.message_card(ui, &card, first)
    }

    /// One conversation's own card in the folder list: the newest mail's sender,
    /// date and preview under the thread's subject, the count of what the thread
    /// holds beside the date, and the chevron at the left that opens it. It is
    /// selected — and struck as unread — the way the mail it speaks for is, so
    /// the row and the pane never disagree about what is open.
    fn thread_row(
        &self,
        ui: &mut egui::Ui,
        thread: &Thread,
        messages: &[MailMessage],
        open: bool,
        first: bool,
    ) -> RowResponse {
        let newest = &messages[thread.newest];
        let uids: Vec<&str> = thread
            .messages
            .iter()
            .map(|&index| messages[index].uid.as_str())
            .collect();
        let checkbox = self.folder_checkbox(&uids);
        let selected = self
            .selected_message
            .as_deref()
            .is_some_and(|uid| thread.holds(messages, uid))
            || checkbox == Some(CheckState::On);
        let preview = if newest.preview.is_empty() {
            " ".to_string()
        } else {
            newest.preview.clone()
        };
        let (account, folder) = self.list_folder();
        // The conversation's box speaks for every mail it holds: `folder_checkbox`
        // says which of them are checked, and `Partial` is the box that stands
        // for a thread whose mails were checked one by one under it. Only a
        // thread that is wholly in fills its row; a half-checked one keeps the
        // plain surface and says so with the dash.
        let card = CardText {
            from: &newest.from,
            title: &thread.subject,
            date: &date_label(newest.timestamp, &newest.date),
            third: &preview,
            unread: thread.unread > 0,
            selected,
            flagged: thread.flagged,
            has_attachments: thread.has_attachments,
            place: RowPlace::Thread {
                open,
                count: thread.messages.len(),
                key: &thread.key,
            },
            checkbox,
            // The thread's own key, not its mails': the conversation's row and
            // the oldest mail under it can be the same uid, and two widgets may
            // not share one id.
            checkbox_id: row_widget_id("thread-checkbox", account, folder, &thread.key),
            delete_id: row_widget_id("thread-delete", account, folder, &thread.key),
            flag_id: row_widget_id("thread-flag", account, folder, &thread.key),
        };
        self.message_card(ui, &card, first)
    }

    /// One search result: the same card, with where the mail lives and the line
    /// the query matched as its third line.
    fn search_row(&self, ui: &mut egui::Ui, hit: &SearchHit, first: bool) -> RowResponse {
        let checkbox = self.hit_checkbox(hit);
        let selected = (self.selected_message.as_deref() == Some(hit.uid.as_str())
            && self.selected_folder.as_deref() == Some(hit.folder.as_str()))
            || checkbox == Some(CheckState::On);
        let account = self
            .accounts
            .iter()
            .find(|account| account.id == hit.account_id)
            .map(|account| account.name.as_str())
            .unwrap_or(hit.account_id.as_str());
        let folder = crate::protocol::imap::decode_mailbox_name(&hit.folder);
        let third = format!("{account} · {folder} — {}", hit.snippet);
        let card = CardText {
            from: &hit.from,
            title: &hit.subject,
            date: &date_label(hit.timestamp, &hit.date),
            third: &third,
            unread: !hit.seen,
            selected,
            flagged: hit.flagged,
            // The row draws the same paperclip a folder row does, so a hit says
            // what the mail carries the one way the whole list says it.
            has_attachments: hit.attachments > 0,
            // Search results are a flat list of hits: where a hit sits in its
            // own folder's conversations is not what was asked for.
            place: RowPlace::Plain,
            checkbox,
            // A hit names the folder it lives in, which is what keeps two
            // accounts' uids apart on one screen.
            checkbox_id: row_widget_id("mail-checkbox", &hit.account_id, &hit.folder, &hit.uid),
            delete_id: row_widget_id("mail-delete", &hit.account_id, &hit.folder, &hit.uid),
            flag_id: row_widget_id("mail-flag", &hit.account_id, &hit.folder, &hit.uid),
        };
        self.message_card(ui, &card, first)
    }

    /// One card in the middle column: avatar on the left; sender + timestamp,
    /// title and a muted third line stacked on the right. The folder list and
    /// the search results share it — only what the three lines say differs —
    /// and `first` marks the card the list starts with.
    fn message_card(&self, ui: &mut egui::Ui, card: &CardText, first: bool) -> RowResponse {
        let scheme = theme::of(ui.ctx());
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), MESSAGE_ROW_HEIGHT),
            egui::Sense::click(),
        );

        // Where the row's avatar sits — and with it the box select mode puts in
        // its place: the two are one slot, so the column of boxes lines up with
        // the column of circles. Where the row sits in the list is the other
        // half of that (`RowPlace::indent`).
        let indent = card.place.indent();
        let (diameter, initial) = card.place.avatar();
        let avatar_center =
            egui::pos2(rect.left() + 8.0 + indent + diameter / 2.0, rect.center().y);
        let box_rect =
            egui::Rect::from_center_size(avatar_center, egui::Vec2::splat(CHECKBOX_SIZE));
        // The box is a target on every row, drawn or not: egui hit-tests a pass
        // against the widget rects of the previous one, so a target that only
        // appears once the pointer is already on it is a target a click can
        // miss.
        let checkbox = ui.interact(
            egui::Rect::from_center_size(
                avatar_center,
                egui::Vec2::splat(CHECKBOX_SIZE + CHECKBOX_PADDING * 2.0),
            ),
            card.checkbox_id,
            egui::Sense::click(),
        );
        // Where the row's text runs, and the cell its own delete button takes
        // when the pointer is on the row: the timestamp's slot at the right end
        // of the first line, which gives way to it.
        let text_left = avatar_center.x + diameter / 2.0 + 10.0;
        let text_right = rect.right() - 10.0;
        let delete_rect = egui::Rect::from_center_size(
            egui::pos2(
                text_right - ROW_ACTION_SIZE / 2.0,
                rect.top() + ROW_SENDER_Y,
            ),
            egui::Vec2::splat(ROW_ACTION_SIZE),
        );
        // The flag button sits just inside it — one cell further from the row's
        // edge — so delete stays outermost, as it is in the title bar and on the
        // mail's own head.
        let flag_rect = delete_rect.translate(egui::vec2(-(ROW_ACTION_SIZE + ROW_ACTION_GAP), 0.0));
        // A target on every row, like the box: egui hit-tests a pass against the
        // widget rects of the previous one, so a target that only appears once
        // the pointer is already on it is a target a click can miss.
        let delete = ui.interact(delete_rect, card.delete_id, egui::Sense::click());
        let flag = ui.interact(flag_rect, card.flag_id, egui::Sense::click());
        // The pointer on one of the row's controls is the pointer on the row:
        // they are controls inside the row, not holes in it.
        let hovered = resp.hovered() || checkbox.hovered() || delete.hovered() || flag.hovered();

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
        } else if hovered {
            painter
                .with_clip_rect(column)
                .rect_filled(full, 0.0, scheme.hover);
        } else if card.flagged {
            // The warning fill a flag is read by. Last of the three: the
            // selection and the pointer are about what the reader is doing
            // *now*, and their fills are stronger than the mark on the mail —
            // which is still said by the bar's own flag cell, in warning color,
            // and by the mail's card in the pane.
            painter
                .with_clip_rect(column)
                .rect_filled(full, 0.0, scheme.flag);
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

        // The room the row's inset leaves at the left is where the chevron is
        // drawn: a conversation's own row and the mails it holds are stepped in,
        // and the chevron's column is reserved on every row.
        let mut chevron = None;
        match card.place {
            RowPlace::Thread { open, key, .. } => {
                let centre = egui::pos2(
                    rect.left() + (THREAD_INDENT - THREAD_CHEVRON) / 2.0,
                    rect.center().y,
                );
                // The chevron is a target of its own: the row's click opens the
                // conversation in the reading pane, and the chevron is what
                // opens the mails under it in the list. It is a line drawing
                // rather than the filled triangle a folder section has, and it
                // takes the pointer through a widget with a hover fill to say
                // so.
                let target =
                    egui::Rect::from_center_size(centre, egui::Vec2::splat(THREAD_CHEVRON + 8.0));
                let response = ui.interact(
                    target,
                    egui::Id::new(("thread-chevron", key)),
                    egui::Sense::click(),
                );
                if response.hovered() {
                    painter.rect_filled(target, 4.0, scheme.hover);
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                // Thin and muted at rest, and the row's own weak color under
                // the pointer: it is a control, not part of the mail.
                let color = if response.hovered() {
                    scheme.text_weak
                } else {
                    scheme.text_weak.gamma_multiply(THREAD_CHEVRON_ALPHA)
                };
                paint_chevron(painter, centre, THREAD_CHEVRON, open, color);
                chevron = Some(response);
            }
            // A mail of an open conversation draws nothing in that column: it
            // says which conversation it belongs to with the step in and the
            // smaller avatar below, not with a line.
            RowPlace::InThread | RowPlace::Plain => {}
        }

        // The box, or the avatar it stands in front of: select mode draws one
        // on every row, and in view mode the pointer brings one over the row it
        // is on — which is the way into the mode. The avatar is deterministic
        // color per sender with the initial centered in it, a smaller one for a
        // mail of an open conversation, which with the step the row is inset by
        // is what says at a glance that the mail belongs to the conversation
        // above it.
        match card.checkbox.or(hovered.then_some(CheckState::Off)) {
            Some(state) => {
                if checkbox.hovered() {
                    // The target filled the way the chevron's is: what says the
                    // box is a control rather than a glyph on the row.
                    painter.rect_filled(checkbox.rect, CHECKBOX_CORNER_RADIUS, scheme.hover);
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                paint_checkbox(painter, box_rect, state, checkbox.hovered(), scheme);
            }
            None => {
                painter.circle_filled(
                    avatar_center,
                    diameter / 2.0,
                    sender_color(scheme, card.from),
                );
                painter.text(
                    avatar_center,
                    egui::Align2::CENTER_CENTER,
                    sender_initial(card.from),
                    egui::FontId::proportional(initial),
                    scheme.avatar_text,
                );
            }
        }

        // Sender and subject always take the body color — read mail is not
        // muted — and unread mail is set heavier instead, so the two states
        // stay apart without dimming half the list. Only the preview below is
        // muted, always.
        let title_color = scheme.text;

        // First row: timestamp right-aligned, sender truncated before it — and,
        // for a conversation, the count of what it holds in front of the date.
        // Under the pointer the row's own delete button takes that end of the
        // line: what is *laid out* here does not change with the hover — the
        // sender is elided against the timestamp's place either way — only what
        // is painted does, so nothing on the row moves under the pointer.
        let mut line_right = text_right;
        if let RowPlace::Thread { count, .. } = card.place {
            let small = egui::TextStyle::Small.resolve(ui.style());
            let badge = ui
                .ctx()
                .fonts_mut(|f| f.layout_no_wrap(count.to_string(), small, scheme.text_weak));
            line_right -= badge.size().x;
            if !hovered {
                painter.galley(
                    egui::pos2(line_right, rect.top() + ROW_SENDER_Y - badge.size().y * 0.5),
                    badge,
                    scheme.text_weak,
                );
            }
            line_right -= 8.0;
        }
        let date_galley = ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap(
                card.date.to_string(),
                egui::FontId::proportional(ROW_DATE_FONT_SIZE),
                scheme.text_weak,
            )
        });
        let date_pos = egui::pos2(
            line_right - date_galley.size().x,
            rect.top() + ROW_SENDER_Y - date_galley.size().y * 0.5,
        );
        if !hovered {
            painter.galley(date_pos, date_galley, scheme.text_weak);
        }
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
            egui::pos2(text_left, rect.top() + ROW_SENDER_Y - sender.size().y * 0.5),
            sender,
            title_color,
            card.unread,
        );

        // Second row: subject. The end of this line is where a row's two marks
        // sit: the flag a marked mail carries, and — outermost, because a mail
        // can carry both — the clip a mail with parts carries. Each takes its
        // own width and a gap, and the title is elided against what is left of
        // the line rather than drawn under them.
        let clip_left = text_right - ROW_CLIP_WIDTH;
        let flag_left = if card.has_attachments {
            clip_left - ROW_CLIP_GAP - ROW_FLAG_WIDTH
        } else {
            clip_left
        };
        let title_right = match (card.flagged, card.has_attachments) {
            (true, _) => flag_left - ROW_CLIP_GAP,
            (false, true) => clip_left - ROW_CLIP_GAP,
            (false, false) => text_right,
        };
        let subject = truncate_galley(
            ui.ctx(),
            card.title,
            ROW_TITLE_FONT_SIZE,
            title_color,
            (title_right - text_left).max(40.0),
        );
        paint_row_text(
            painter,
            egui::pos2(
                text_left,
                rect.top() + ROW_SUBJECT_Y - subject.size().y * 0.5,
            ),
            subject,
            title_color,
            card.unread,
        );
        if card.flagged {
            // The mark its warning fill is about, in the mark's own colour: the
            // fill says *something* about this mail, and this says what — and it
            // is the whole of what a reader who cannot tell one fill from
            // another has to go on.
            paint_flag_icon(
                painter,
                egui::Rect::from_center_size(
                    egui::pos2(flag_left + ROW_FLAG_WIDTH / 2.0, rect.top() + ROW_SUBJECT_Y),
                    egui::vec2(ROW_FLAG_SIZE, ROW_FLAG_SIZE),
                ),
                scheme.warn,
                ROW_FLAG_SIZE,
            );
        }
        if card.has_attachments {
            // Always muted, and independent of the unread weight: what the mail
            // carries is not part of its title.
            paint_clip_icon(
                painter,
                egui::Rect::from_center_size(
                    egui::pos2(
                        text_right - ROW_CLIP_WIDTH / 2.0,
                        rect.top() + ROW_SUBJECT_Y,
                    ),
                    egui::vec2(ROW_CLIP_SIZE, ROW_CLIP_SIZE),
                ),
                scheme.text_weak,
            );
        }

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
            egui::pos2(text_left, rect.top() + ROW_PREVIEW_Y - third.size().y * 0.5),
            third,
            scheme.text_weak,
        );

        // The row's own two buttons, in the slot the timestamp gave up. The
        // flag comes first — it is the inner of the pair — and the state it
        // switches is on the mail already: the row's warning fill and the mark
        // at the end of its title line. So its glyph wears the mark's own colour
        // while the mail is flagged, and turning the pointer on the cell
        // previews the mark by filling it with the tint the flagged row itself
        // is filled with.
        if hovered {
            let flag_color = if flag.hovered() || card.flagged {
                scheme.warn
            } else {
                scheme.text_weak
            };
            if flag.hovered() {
                painter.rect_filled(flag_rect, ROW_ACTION_CORNER_RADIUS, scheme.flag);
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            paint_flag_icon(painter, flag_rect, flag_color, ROW_ACTION_GLYPH);

            // The delete button beside it: a muted glyph while the pointer is
            // anywhere on the row, and the scheme's danger fill under the glyph
            // once the pointer is on the button itself — the treatment the title
            // bar's close button gets, and for the same reason: the button is the
            // confirmation, and what it does cannot be undone.
            if delete.hovered() {
                painter.rect_filled(delete_rect, ROW_ACTION_CORNER_RADIUS, scheme.danger_hover);
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                paint_delete_icon(painter, delete_rect, scheme.on_danger, ROW_ACTION_GLYPH);
            } else {
                paint_delete_icon(painter, delete_rect, scheme.text_weak, ROW_ACTION_GLYPH);
            }
        }

        let delete = delete.on_hover_text(row_delete_tooltip(card.place));
        let flag = flag.on_hover_text(row_flag_tooltip(card.place, card.flagged));
        RowResponse {
            row: resp,
            chevron,
            checkbox,
            delete,
            flag,
        }
    }

    /// Right column: the selected mail on the card the reading pane floats on —
    /// the lighter content surface, inset in the chrome by CONTENT_GUTTER,
    /// whose left side is also the splitter. With nothing selected there is no
    /// card at all: the pane is bare chrome and the placeholder sits directly on
    /// it.
    ///
    /// A conversation draws one such card per mail (`mail_card`), the cards
    /// stacked with chrome showing between them, rather than one card holding a
    /// card per mail: the pane's own surface, once per mail, so a thread reads
    /// as the mails it is made of and each of them sits on the pane the way a
    /// mail of its own does. The compose a mail's own header opens sits in
    /// the same stack — the card right under the mail it answers
    /// (`compose_inline_card`).
    ///
    /// The cards share the pane's scroll container: the headers, the rule and
    /// the body of a mail are one column on its card, so the scroll bar runs
    /// down the pane's edge rather than down a card's. The cards reach the
    /// window's top edge — the floating title bar lies over them — and the first
    /// one's top padding is the bar's height (see `mail_card`), so a mail
    /// scrolls under the glass exactly like the message list's rows do.
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
        let Some(conversation) = self.open_conversation() else {
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

        // A newly opened conversation starts at the top. The scroll offset is
        // per pane, not per mail, so without this the next one opens wherever
        // the previous one was left — for a Blitz page that means the top of
        // the page is simply not on screen. It is keyed by the conversation and
        // not by the mail: opening another mail of the thread already on screen
        // is not a reason to scroll away from where the reader was.
        let message_key = format!(
            "{}/{}",
            self.selected_folder.as_deref().unwrap_or("-"),
            conversation.key
        );
        let new_message = self.scrolled_message.as_deref() != Some(message_key.as_str());
        if new_message {
            self.scrolled_message = Some(message_key);
        }

        // The cards are laid out with the mails and filled in below, once the
        // mails have been laid out: a card hugs its mail, so its height is only
        // known then. The pane draws them itself rather than framing each mail,
        // because a card is the pane's own surface: as wide as the pane, so a
        // mail wider than the pane — most HTML mail — scrolls sideways over its
        // card instead of dragging the card off the pane's edge with it.
        let card_left = panel.left() + CONTENT_GUTTER;
        let card_right = panel.right() - CONTENT_GUTTER;
        let card_top = panel.top() + CONTENT_GUTTER;
        let card_bottom = panel.bottom() - CONTENT_GUTTER;
        let inner = egui::Rect::from_min_max(
            egui::pos2(card_left, card_top),
            egui::pos2(card_right, card_bottom),
        );
        // One entry per mail: the card's reserved shape, and the rect the mail
        // came to rest in, which is what the card is drawn around below.
        let mut cards = Vec::new();
        ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
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
            // A compose opened on the spot sits in this stack, as one more
            // card right under the mail it answers (`compose_about`, called
            // from the mail's own header). `ui` keeps the card and its mail
            // together: a draft whose mail has left the pane moves to the
            // popup window before the pane draws (`compose_anchor_on_pane`),
            // so the uid waited for here is a uid of this conversation.
            let answer_under = if self.compose.is_some() && self.compose_spot == ComposeSpot::Pane {
                self.compose_anchor
                    .as_ref()
                    .map(|anchor| anchor.uid.clone())
            } else {
                None
            };
            let output = scroll.show(ui, |ui| {
                // One card per mail, oldest first — a mail of its own is
                // the same card, drawn once. `add_space` leaves egui's own
                // item spacing on top of what it is asked for, so the room
                // between two cards asks for the gap minus that
                // (`pane_card_gap`).
                for (index, mail) in conversation.messages.iter().enumerate() {
                    if index == 0 {
                        // The pane's first card starts *below* the floating title
                        // bar rather than behind it — `CONTENT_GUTTER` under the
                        // bar, like the card's other three sides — so the card's
                        // top edge is on screen, rounded corners and all. The
                        // room is space in the scroll area and not padding inside
                        // the card, which is what lets it scroll away with the
                        // card: the mail still passes under the glass.
                        // The room is asked for as it is: nothing has been laid
                        // out above this card, so there is no item spacing
                        // under it to make up for (see the gap below). The
                        // popup layout is no different: its back button
                        // floats over this room rather than claiming it.
                        ui.add_space(TITLEBAR_HEIGHT);
                    } else {
                        // The chrome showing between two cards of a conversation.
                        ui.add_space(pane_card_gap(ui));
                    }
                    cards.push(self.mail_card(ui, mail));
                    if answer_under.as_deref() == Some(mail.uid.as_str()) {
                        // The answer to this mail, with the pane's own gap
                        // above it; the thread's next mail, if any, adds its
                        // own below.
                        ui.add_space(pane_card_gap(ui));
                        cards.push(self.compose_inline_card(ui));
                    }
                }
            });
            // Scrolled away from the top, the bar floats over the cards.
            self.mail_scroll = output.state.offset.y;
        });
        // The fills, in behind the mails: a card spans the pane and stays on it
        // while the mail scrolls, and it is clipped to the pane, so a card
        // scrolled up under the title bar is cut at the pane's top edge rather
        // than painted over the chrome's gutter.
        let painter = ui.painter().with_clip_rect(inner);
        for (where_to_put, laid_out) in cards {
            painter.set(
                where_to_put,
                egui::Shape::rect_filled(
                    egui::Rect::from_x_y_ranges(card_left..=card_right, laid_out.y_range()),
                    CONTENT_CARD_CORNER_RADIUS,
                    scheme.content,
                ),
            );
        }

        self.popup_back(ui, panel);
    }

    /// The compose card as one more card of the pane's stack, right under
    /// the mail it answers (`mail_content`): the popup's own card
    /// (`compose_card`) on the pane's surface, at the pane's width rather
    /// than the popup's fixed one. The mail it answers stays on screen above
    /// it, and the card scrolls with the thread it belongs to; its body
    /// scrolls on its own (`COMPOSE_BODY_HEIGHT`), so the card is bounded
    /// and the pane's scroll stays the mail's. Reserved as a shape here and
    /// filled in below like a mail's card, because the card's height is only
    /// known once it has been laid out.
    ///
    /// The frame the card is first drawn on is also the one that brings it
    /// into view: a reply cell at the top of a long mail would otherwise open
    /// its card below the fold, with nothing on screen to say the click had
    /// done anything. The draft's `focus` flag — the same first-frame flag the
    /// card's own keyboard follows — is read here *before* `compose_card`
    /// takes it, and the pane then scrolls by no more than the card needs:
    /// with room for it on screen, the mail above gives way by the card's own
    /// height and not a point more, and a card with no room for it all shows
    /// its head (egui aligns the top when a rect cannot fit).
    fn compose_inline_card(&mut self, ui: &mut egui::Ui) -> (egui::layers::ShapeIdx, egui::Rect) {
        let where_to_put = ui.painter().add(egui::Shape::Noop);
        let reveal = self.compose.as_ref().is_some_and(|draft| draft.focus);
        let mut outcome = ComposeOutcome::Shown;
        let laid_out = egui::Frame::new()
            .inner_margin(egui::Margin {
                left: CONTENT_PADDING_I8,
                right: CONTENT_PADDING_I8,
                top: CONTENT_PADDING_I8,
                bottom: CONTENT_PADDING_I8,
            })
            .show(ui, |ui| {
                let width = ui.available_width();
                ui.set_min_width(width);
                // No heading and no separator: the card's own first line is the
                // From list, which is what the answer is *from* — and the mail
                // it answers is right above it, which is what the heading used
                // to say (`compose_card`, `inline`).
                outcome = self.compose_card(ui, width, ComposeSpot::Pane);
            })
            .response
            .rect;
        if reveal {
            ui.scroll_to_rect(laid_out, None);
        }
        self.settle_compose_outcome(outcome);
        (where_to_put, laid_out)
    }

    /// The popup layout's floating back button, whatever the pane is showing:
    /// the way out of the pane to the list under it. A compose open on the
    /// spot goes with it — the card was opened under its mail, and the mail
    /// is what is being left. In the column layout there is no button at
    /// all: the way back is the list itself, always there on the left.
    ///
    /// The button is no bar and no part of the mail: a round, chevron-only
    /// cell floating over the first card's top-left corner, under the title
    /// bar, drawn after whatever the pane drew so a mail scrolled up under it
    /// goes beneath it. What makes it read as floating — as a thing on top of
    /// the pane rather than a hole in it — is the shadow behind it, the
    /// window's own, the same one the `Load older mail` button floats on. Its
    /// rest treatment is the attachment card's (`card` fill, border hairline),
    /// and the glyph is the thread rows' chevron mirrored
    /// (`paint_back_chevron`).
    fn popup_back(&mut self, ui: &egui::Ui, panel: egui::Rect) {
        if !self.pane_popup {
            return;
        }
        let scheme = theme::of(ui.ctx());
        let centre = egui::pos2(
            panel.left() + POPUP_BACK_MARGIN + POPUP_BACK_DIAMETER / 2.0,
            panel.top() + TITLEBAR_HEIGHT + POPUP_BACK_MARGIN + POPUP_BACK_DIAMETER / 2.0,
        );
        let rect = egui::Rect::from_center_size(centre, egui::Vec2::splat(POPUP_BACK_DIAMETER));
        let back = ui
            .interact(rect, egui::Id::new("popup-back"), egui::Sense::click())
            .on_hover_text("Back to the message list");
        let painter = ui.painter();
        // The shadow first, so the button sits on it and not in it.
        painter.add(
            ui.visuals()
                .window_shadow
                .as_shape(rect, POPUP_BACK_DIAMETER / 2.0),
        );
        let fill = if back.hovered() {
            scheme.hover
        } else {
            scheme.card
        };
        painter.circle_filled(centre, POPUP_BACK_DIAMETER / 2.0, fill);
        painter.circle_stroke(
            centre,
            POPUP_BACK_DIAMETER / 2.0,
            egui::Stroke::new(1.0_f32, scheme.border),
        );
        paint_back_chevron(painter, centre, scheme.text);
        if back.clicked() {
            // A compose open on the spot goes with the view it was opened in:
            // its mail is on the pane above it. A draft already on its way
            // out is not thrown away by it — its Send is owed an event, and
            // the button waits for it.
            let composing = self.compose.is_some() && self.compose_spot == ComposeSpot::Pane;
            if !composing || !self.compose.as_ref().is_some_and(|draft| draft.sending) {
                if composing {
                    self.compose = None;
                }
                self.selected_message = None;
            }
        }
    }

    /// One mail on the pane: the sheet it draws (`mail_sheet`), padded away
    /// from the card's edges — the card itself is the pane's, reserved as a
    /// shape here for it to fill in below, because the mail's height is only
    /// known once it has been laid out.
    ///
    /// The padding is inside the scroll area, so the scroll bar sits on the
    /// card's edge and not on the text. `first` is the mail at the top of what
    /// the pane shows, whose top padding is the title bar's height on top of the
    /// card's own: the bar floats over the card, and that is what keeps the
    /// subject below it at rest — scrolling takes the padding away with the
    /// headers, which is what lets them pass under the glass. The mails under it
    /// keep the card's own padding alone, and the room between two cards is the
    /// pane's (`CONVERSATION_CARD_GAP`).
    fn mail_card(
        &mut self,
        ui: &mut egui::Ui,
        msg: &MailMessage,
    ) -> (egui::layers::ShapeIdx, egui::Rect) {
        let where_to_put = ui.painter().add(egui::Shape::Noop);
        let laid_out = egui::Frame::new()
            .inner_margin(egui::Margin {
                left: CONTENT_PADDING_I8,
                right: CONTENT_PADDING_I8,
                top: CONTENT_PADDING_I8,
                bottom: CONTENT_PADDING_I8,
            })
            .show(ui, |ui| self.mail_sheet(ui, msg))
            .response
            .rect;
        (where_to_put, laid_out)
    }

    /// What one mail says and nothing around it: its head — the sender first,
    /// with the list's own avatar in front of the name and the mail's own
    /// actions at that row's right end — then the subject, the addresses and
    /// the date, the rule under them, the body, and the cards of what it came
    /// with. The pane pads every card (`mail_card`), which is why the
    /// padding is not here.
    fn mail_sheet(&mut self, ui: &mut egui::Ui, msg: &MailMessage) {
        ui.set_min_width(ui.available_width());
        // The mail's own column: the headers draw across it and so do the
        // attachment cards at the foot. Taken before anything is laid out,
        // because a body wider than the pane — a fixed-width HTML table —
        // widens the ui as it is drawn, and the cards belong to the mail, not
        // to what the mail overflowed by.
        let column = ui.available_width();
        // The mail's head: the sender first, with the list's own avatar in
        // front of them, and the mail's own actions at that row's right end —
        // the title bar's run brought down onto the mail itself. Reply, reply
        // all and forward open their compose right here (the card comes up
        // under this mail, `ComposeSpot::Pane`), and delete takes this one
        // mail, the same `Job::DeleteMessage` the row's own button sends. The
        // label cell opens the label popup on this mail, so a mail is labelled
        // where it is read. The cells are the bar's own
        // (`card_action_button` — the bar's button in a smaller square — and
        // the bar's glyphs), so the two runs read as one family, delete
        // outermost as it is in the bar, and the five are the bar's run with
        // flag and export left out. The cells are not the bar's size: on a card
        // they share their row with the sender, and a 32 pt square would make
        // the row read as a piece of the bar rather than of the mail.
        //
        // The row is laid out from its right end, so the actions take the room
        // they need and the name is elided against what is left: laid out from
        // the left, a long sender would push the actions off the card. The
        // avatar is the message list's own — its circle, the sender's color on
        // it and their initial — so a mail's head reads as one of the list's
        // rows lifted onto the pane.
        let scheme = theme::of(ui.ctx());
        let mut answer = None;
        let mut delete = false;
        let row_height = AVATAR_DIAMETER.max(CARD_ACTION_SIZE);
        let (head, _) =
            ui.allocate_exact_size(egui::vec2(column, row_height), egui::Sense::hover());
        // A scope of the row's own height, so the two layouts inside it
        // centre in the row — not in what the card has left below it.
        ui.scope_builder(egui::UiBuilder::new().max_rect(head), |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = TITLEBAR_BUTTON_GAP;
                for action in [
                    MailAction::Delete,
                    MailAction::Label,
                    MailAction::Forward,
                    MailAction::ReplyAll,
                    MailAction::Reply,
                ] {
                    let response = card_action_button(ui, true);
                    let color = card_action_color(scheme, response.hovered());
                    let painter = ui.painter();
                    match action {
                        MailAction::Reply => {
                            paint_reply_icon(painter, response.rect, color, CARD_ACTION_GLYPH)
                        }
                        MailAction::ReplyAll => {
                            paint_reply_all_icon(painter, response.rect, color, CARD_ACTION_GLYPH)
                        }
                        MailAction::Forward => {
                            paint_forward_icon(painter, response.rect, color, CARD_ACTION_GLYPH)
                        }
                        MailAction::Label => {
                            paint_label_icon(painter, response.rect, color, CARD_ACTION_GLYPH)
                        }
                        MailAction::Delete => {
                            paint_delete_icon(painter, response.rect, color, CARD_ACTION_GLYPH)
                        }
                        _ => unreachable!("the five the head draws"),
                    }
                    let hovered = response.on_hover_text(action.tooltip());
                    if action == MailAction::Label {
                        // The card is the mail's own head, so labelling happens
                        // here the way answering does: the popup about this
                        // mail, opened from the cell on it.
                        if let Some(key) = self.pane_mail_key(&msg.uid) {
                            if hovered.clicked() {
                                self.label_draft.clear();
                            }
                            self.label_popup(&hovered, &key);
                        }
                    } else if hovered.clicked() {
                        if action == MailAction::Delete {
                            delete = true;
                        } else {
                            answer = Some(action);
                        }
                    }
                }
                // The paper cell, when this card is drawing a Blitz page: the
                // one control on the head that is about the *view* rather than
                // about the mail, which is why it is drawn last — it sits at the
                // run's inner end, nearest the sender, and the five mail actions
                // keep their order with delete outermost, as the title bar has
                // it. A mail whose own HTML paints a background is not affected
                // by it (the renderer paints that over the paper), and the cell
                // is not hidden for one: nothing here reads the mail's CSS.
                if self.html_renderer == HtmlRenderer::Blitz && msg.body_html.is_some() {
                    let key = self.pane_mail_key(&msg.uid);
                    let paper = key
                        .as_ref()
                        .map(|key| self.paper_of(key))
                        .unwrap_or_default();
                    let response = card_action_button(ui, true);
                    let color = card_action_color(scheme, response.hovered());
                    paint_paper_icon(ui.painter(), response.rect, color, CARD_ACTION_GLYPH);
                    if response.on_hover_text(paper.tooltip()).clicked() {
                        if let Some(key) = key {
                            self.toggle_blitz_paper(&key);
                        }
                    }
                }
                // What the actions leave of the row is the sender.
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = AVATAR_TEXT_GAP;
                    let (avatar, _) = ui.allocate_exact_size(
                        egui::Vec2::splat(AVATAR_DIAMETER),
                        egui::Sense::hover(),
                    );
                    let painter = ui.painter();
                    painter.circle_filled(
                        avatar.center(),
                        AVATAR_DIAMETER / 2.0,
                        sender_color(scheme, &msg.from),
                    );
                    painter.text(
                        avatar.center(),
                        egui::Align2::CENTER_CENTER,
                        sender_initial(&msg.from),
                        egui::FontId::proportional(AVATAR_FONT_SIZE),
                        scheme.avatar_text,
                    );
                    // A long sender is cut with an ellipsis: the head is one
                    // row, and it is the actions' row.
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    ui.label(egui::RichText::new(msg.from.as_str()).strong());
                });
            });
        });
        if let Some(action) = answer {
            self.compose_about(action, ComposeSpot::Pane, msg);
        }
        if delete {
            let (Some(account_id), Some(folder)) =
                (self.selected_account.clone(), self.selected_folder.clone())
            else {
                return;
            };
            self.delete_mail(&account_id, &folder, &msg.uid);
        }
        // What the user has marked the mail with, on the mail itself: the label
        // popup's checkmarks say it while it is open, and these say it after it
        // has closed. A mail nobody labelled carries no row at all.
        if !msg.labels.is_empty() {
            ui.add_space(LABEL_CHIPS_TOP_GAP);
            label_chips(ui, scheme, &msg.labels);
        }
        ui.heading(msg.subject.as_str());
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
        // The rule between the headers and the body spans the card, not just the
        // mail's text column.
        ui.add(egui::Separator::default().grow(CONTENT_PADDING.x));
        self.mail_body(ui, msg);
        self.attachment_cards(ui, msg, column);
    }

    /// What the mail came with: one card per attachment, under the body.
    ///
    /// The cards live here rather than in `mail_body` because both renderers
    /// have to have them — a Blitz body is a raster that a worker made, and the
    /// cards are not part of it — and because they are the last thing on the
    /// card, below the body whichever way the body was drawn. They are one run
    /// across the mail's own column, wrapped where the next card does not fit:
    /// each as wide as the name it carries wants (`attachment_card`), so what a
    /// mail came with reads as a row of parts rather than as one card per line,
    /// and a pane too narrow for the whole run wraps instead of cutting it.
    fn attachment_cards(&mut self, ui: &mut egui::Ui, msg: &MailMessage, column: f32) {
        if msg.attachments.is_empty() {
            return;
        }
        // The click is acted on once the cards are drawn, so that the borrow of
        // the mail the pane is showing is over before the job is started.
        let mut clicked = None;
        ui.add_space(ATTACHMENT_CARDS_TOP_GAP);
        // The run is laid out in a scope of its own, which is what holds its
        // width to the column the headers were drawn across: a body wider than
        // that column (a fixed-width HTML table) widens this ui as it lays
        // itself out, and the cards belong to the mail, not to what the mail
        // overflowed by (see `mail_body`).
        let start = ui.cursor().min;
        let run =
            egui::Rect::from_min_max(start, egui::pos2(start.x + column, ui.max_rect().bottom()));
        ui.scope_builder(egui::UiBuilder::new().max_rect(run), |ui| {
            // Between two cards, and between two rows of them.
            ui.spacing_mut().item_spacing = egui::Vec2::splat(ATTACHMENT_CARD_GAP);
            ui.horizontal_wrapped(|ui| {
                for (index, attachment) in msg.attachments.iter().enumerate() {
                    let card =
                        attachment_card(ui, attachment, column, "Click to fetch this file", false);
                    if card.card.clicked() {
                        clicked = Some(index);
                    }
                }
            });
        });
        if let Some(index) = clicked {
            let name = msg.attachments[index].name.clone();
            self.download_attachment(&msg.uid, index, &name);
        }
    }

    /// Download one attachment of the mail the reading pane shows: a worker
    /// fetches the mail's original source from the server, takes that part out
    /// of it and writes the file into the downloads directory
    /// (`Job::DownloadAttachment`, `storage::downloads_dir`).
    ///
    /// `ordinal` is the card's position in `msg.attachments`, which is the
    /// order the parts came in and so the order the database stored them in —
    /// the worker re-parses the original and finds the part again by it and by
    /// `name`.
    ///
    /// The round trip is the price of never storing the bytes, exactly as it is
    /// for an export: mail the server no longer has cannot be downloaded, and
    /// that comes back on the account (`Event::SyncFailed`, prefixed
    /// "attachment:"). What went right is the same timed line an export uses
    /// (`Event::AttachmentDownloaded`).
    fn download_attachment(&mut self, uid: &str, ordinal: usize, name: &str) {
        let (Some(account_id), Some(folder)) =
            (self.selected_account.clone(), self.selected_folder.clone())
        else {
            return;
        };
        let Some(account) = self.accounts.iter().find(|a| a.id == account_id).cloned() else {
            return;
        };
        sync::spawn(
            Job::DownloadAttachment {
                account,
                folder,
                uid: uid.to_string(),
                ordinal,
                name: name.to_string(),
                dir: storage::downloads_dir(),
            },
            self.ctx.clone(),
            self.tx.clone(),
        );
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
                // Where it reads from, and where it sends through when it can
                // send at all.
                let mut line = format!(
                    "{} · {}:{} · {}",
                    account.protocol.label(),
                    account.host,
                    account.port,
                    account.username
                );
                if let Some(smtp) = &account.smtp {
                    line.push_str(&format!(" · sends via {}:{}", smtp.host, smtp.port));
                }
                ui.label(egui::RichText::new(line).small().weak());
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
                ui.add(Field::new(
                    egui::TextEdit::singleline(&mut f.name).hint_text("Personal mail"),
                ));
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
                ui.add(Field::new(
                    egui::TextEdit::singleline(&mut f.host).hint_text("imap.example.com"),
                ));
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
                ui.add(Field::new(
                    egui::TextEdit::singleline(&mut f.port_str).hint_text("993"),
                ));
                ui.end_row();

                ui.label("Username");
                ui.add(Field::new(
                    egui::TextEdit::singleline(&mut f.username).hint_text("you@example.com"),
                ));
                ui.end_row();
            });

        ui.add_space(8.0);
        // Sending is its own group, and off is a real answer: an account that
        // only reads mail is what every account was before this existed.
        ui.checkbox(&mut f.smtp_enabled, "Send mail (SMTP)");
        ui.label(
            egui::RichText::new(
                "Mail is sent through the server below. Without one, this account can only receive.",
            )
            .small()
            .weak(),
        );
        ui.add_space(4.0);
        ui.add_enabled_ui(f.smtp_enabled, |ui| {
            egui::Grid::new("account_smtp_grid")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Outgoing server");
                    ui.add(Field::new(
                        egui::TextEdit::singleline(&mut f.smtp_host).hint_text("smtp.example.com"),
                    ));
                    ui.end_row();

                    ui.label("Security");
                    ui.horizontal(|ui| {
                        let mut changed = false;
                        if ui
                            .radio_value(&mut f.smtp_tls, TlsMode::Implicit, "TLS")
                            .changed()
                        {
                            changed = true;
                        }
                        if ui
                            .radio_value(&mut f.smtp_tls, TlsMode::StartTls, "STARTTLS")
                            .changed()
                        {
                            changed = true;
                        }
                        if ui
                            .radio_value(&mut f.smtp_tls, TlsMode::Plain, "None")
                            .changed()
                        {
                            changed = true;
                        }
                        if changed {
                            f.smtp_port_str = SmtpSettings::default_port(f.smtp_tls).to_string();
                        }
                    });
                    ui.end_row();

                    ui.label("Port");
                    ui.add(Field::new(
                        egui::TextEdit::singleline(&mut f.smtp_port_str).hint_text("587"),
                    ));
                    ui.end_row();

                    ui.label("Login");
                    ui.vertical(|ui| {
                        ui.checkbox(
                            &mut f.smtp_own_login,
                            "Use this account's username and password",
                        );
                        if !f.smtp_own_login {
                            ui.horizontal(|ui| {
                                ui.add(Field::new(
                                    egui::TextEdit::singleline(&mut f.smtp_username)
                                        .hint_text("Username")
                                        .desired_width(150.0),
                                ));
                                ui.add(Field::new(
                                    egui::TextEdit::singleline(&mut f.smtp_password)
                                        .hint_text("Password")
                                        .password(true)
                                        .desired_width(150.0),
                                ));
                            });
                        }
                    });
                    ui.end_row();
                });
            if f.smtp_tls == TlsMode::Plain {
                ui.label(
                    egui::RichText::new("A plain connection sends the password in the clear.")
                        .small()
                        .weak(),
                );
            }
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

    /// Preferences → General: the mail list, the automatic sync and the
    /// notifications, all persisted on change.
    fn general_page(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Mail list").strong());
        if ui
            .checkbox(&mut self.threaded, "Group mail into conversations")
            .changed()
        {
            self.persist_settings();
        }
        ui.label(
            egui::RichText::new(
                "A conversation is one row, showing its newest mail; click it to open \
                 the mails it holds. Replies are grouped by their References and \
                 In-Reply-To headers, and — for mail cached before Kimo kept those — by \
                 a subject that answers another.",
            )
            .small()
            .weak(),
        );

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        ui.label(egui::RichText::new("Automatic sync").strong());
        if ui
            .checkbox(&mut self.auto_sync, "Sync mail automatically")
            .changed()
        {
            // The clock starts now: switching it on does not mean syncing this
            // instant, and switching it off and on again is not a retry.
            self.auto_synced_at = Instant::now();
            self.persist_settings();
        }
        let mut minutes = (self.auto_sync_every.as_secs() / SECS_PER_MINUTE) as u32;
        ui.horizontal(|ui| {
            ui.add_enabled_ui(self.auto_sync, |ui| {
                ui.label("Every");
                if ui
                    .add(
                        egui::DragValue::new(&mut minutes)
                            .range(AUTO_SYNC_MIN_MINUTES..=AUTO_SYNC_MAX_MINUTES)
                            .suffix(" min"),
                    )
                    .changed()
                {
                    self.auto_sync_every = auto_sync_interval(Some(minutes));
                    self.auto_synced_at = Instant::now();
                    self.persist_settings();
                }
                ui.label("apart, while Kimo is running");
            });
        });
        ui.label(
            egui::RichText::new(
                "Every account is asked for its mail on that clock, and only while no \
                 sync is already running. Kimo syncs once at startup either way, and \
                 the title bar's Sync button asks for the same sweep at any time — as \
                 does opening a folder that was never fetched.",
            )
            .small()
            .weak(),
        );

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        ui.label(egui::RichText::new("Notifications").strong());
        if ui
            .checkbox(
                &mut self.notifications,
                "Show a desktop notification for new mail",
            )
            .changed()
        {
            self.persist_settings();
        }
        ui.label(
            egui::RichText::new(
                "A sync that finds mail which arrived since the last one announces it: \
                 one notification per mail, or one counting them when a batch is large. \
                 Mail in the folders it is filed into rather than delivered to — Sent, \
                 Drafts, Trash, Junk, and Gmail's All Mail and Starred — stays quiet.",
            )
            .small()
            .weak(),
        );
        ui.label(
            egui::RichText::new(
                "Linux desktops show these through org.freedesktop.Notifications on the \
                 session bus; a session without a notification daemon simply stays quiet.",
            )
            .small()
            .weak(),
        );
    }

    /// Preferences → Appearance: theme, accent and HTML renderer, all
    /// persisted to settings.json on change.
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

        ui.label(egui::RichText::new("Accent").strong());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = ACCENT_CHIP_GAP;
            for accent in Accent::ALL {
                if accent_chip(ui, self.scheme().dark, accent, self.accent == accent).clicked() {
                    self.accent = accent;
                    self.persist_settings();
                }
            }
        });
        ui.label(
            egui::RichText::new(format!(
                "{} — the colour Kimo marks things with: a focused field's outline, \
                 a checked box, the rows you select.",
                self.accent.name
            ))
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

        color_scheme_preview(ui, self.scheme());
    }
}

/// Preferences → Appearance: the palette in force, so every slot the app draws
/// with can be read at a glance — the theme's own values with the chosen
/// accent among them. Read-only: it documents what the app draws with, and the
/// two slots that *are* configurable are picked above it (the theme's radios
/// and the accent's chips).
fn color_scheme_preview(ui: &mut egui::Ui, scheme: ColorScheme) {
    ui.label(egui::RichText::new("Color scheme").strong());
    ui.label(
        egui::RichText::new(format!("{} — the palette in force.", scheme.name))
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

/// The accent chips of Preferences → Appearance: one square per choice, with
/// room between them for the chosen one's ring, which is drawn just outside the
/// chip.
const ACCENT_CHIP_SIZE: f32 = 20.0;
const ACCENT_CHIP_CORNER_RADIUS: u8 = 5;
const ACCENT_CHIP_GAP: f32 = 10.0;
const ACCENT_CHIP_RING: f32 = 2.0;
const ACCENT_CHIP_RING_GAP: f32 = 3.0;

/// One accent chip in Preferences → Appearance: the colour itself, ringed in
/// the app's own ink while it is the one in force — a ring, because the chip
/// *is* a colour and a mark drawn on it would be a second colour to read — and
/// named on hover, which is what tells two neighbouring blues apart.
///
/// `dark` picks the theme's variant of the accent, so the chips show the
/// colours the app is drawing with rather than the other scheme's.
fn accent_chip(ui: &mut egui::Ui, dark: bool, accent: Accent, chosen: bool) -> egui::Response {
    let size = egui::Vec2::splat(ACCENT_CHIP_SIZE);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let color = accent.colors(dark).accent;
    ui.painter()
        .rect_filled(rect, ACCENT_CHIP_CORNER_RADIUS, color);
    if chosen {
        ui.painter().rect_stroke(
            rect.expand(ACCENT_CHIP_RING_GAP),
            ACCENT_CHIP_CORNER_RADIUS,
            egui::Stroke::new(ACCENT_CHIP_RING, theme::of(ui.ctx()).text),
            egui::StrokeKind::Inside,
        );
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.on_hover_text(accent.name)
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
        self.attach_dropped();
        self.apply_theme(ctx);
        // The automatic sync is a frame's business too: it is what asks for the
        // frame its clock needs, and what starts the sweep when it is up.
        self.pump_auto_sync(ctx);

        // The columns give way to a narrowing window in one order
        // (`plan_columns`): the navigation collapses first, then the reading
        // pane is capped at what leaves the list its minimum, then the pane
        // becomes a popup over the list, and the list itself never narrows
        // past its rows — a window narrower than that scrolls it sideways.
        // The thresholds are the columns' own widths rather than constants:
        // what the splitters left the two panels at, plus the list's minimum,
        // is the total the three need, and the navigation is the one column
        // whose width the two content columns can have for themselves.
        let window = ctx.viewport_rect().width();
        let nav_width = panel_width(ctx, "nav")
            .unwrap_or(NAV_DEFAULT_WIDTH)
            .max(NAV_MIN_WIDTH);
        let plan = plan_columns(window, nav_width, self.pane_width, self.nav_collapsed);
        // Collapse the navigation when the window crosses into having less
        // than the three columns need; re-expanding is a manual choice and
        // stays until the next crossing.
        if plan.collapses_nav && !self.was_narrow {
            self.nav_collapsed = true;
        }
        self.was_narrow = plan.collapses_nav;
        self.pane_popup = plan.popup;
        self.list_scrolls = plan.list_scrolls;

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
        // columns run from the window's top edge and the message list can
        // scroll under the bar. What each column keeps clear of the bar is its
        // own business — the list's top padding scrolls away with its rows
        // (`LIST_PADDING`), the navigation and the reading pane are inset by
        // the bar's height.
        // The reading pane: a column beside the list, or — when the window
        // cannot hold even the two minimums side by side — a popup over it
        // (`mail_content` draws both shapes, and the band with the back
        // button is its own).
        if !plan.popup {
            let mut pane = egui::Panel::right("mail_content")
                .min_size(CONTENT_MIN_WIDTH)
                .default_size(CONTENT_DEFAULT_WIDTH)
                .show_separator_line(false)
                // No frame: the gutter and the card are painted in mail_content.
                .frame(egui::Frame::NONE);
            match plan.pane_max {
                // The window has less than the pane's remembered width beside
                // the list's minimum, so the pane takes what there is. It is
                // placed at the cap outright rather than merely clamped by it:
                // clamped, egui would remember the narrowest width the cap
                // reached and sit there while the window widens through the
                // same phase. The cap is not a split the user can make — the
                // window is the one making it — so the splitter is off too.
                Some(cap) => {
                    set_panel_width(ctx, "mail_content", cap);
                    pane = pane.max_size(cap).resizable(false);
                    self.pane_capped = true;
                }
                None => {
                    // The cap has lifted, and what egui remembers is the last
                    // width it capped at: back to the remembered one, which
                    // the frames below then keep following the splitter.
                    if self.pane_capped {
                        set_panel_width(ctx, "mail_content", self.pane_width);
                        self.pane_capped = false;
                    }
                    if let Some(width) = panel_width(ctx, "mail_content") {
                        self.pane_width = width.max(CONTENT_MIN_WIDTH);
                    }
                }
            }
            pane.show(ui, |ui| self.mail_content(ui));
        }
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
        // In the popup layout the navigation is not shown at all: a window
        // that cannot hold the two content columns side by side has nothing
        // left for a third. The flag survives, and the column is back the
        // moment the window can hold it again.
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
        // No padding on the frame: the column's padding lives inside its scroll
        // area (see folder_messages), so the scroll bar can sit on the column's
        // edge instead of on the padding. The column's own rect — what the two
        // side panels leave — is recorded by `mail_list` for the splitter
        // repaint below.
        //
        // The column's surface is the card `mail_list` paints, as tall as the
        // list it holds, so the panel behind it is the chrome: that is what
        // shows below a folder that does not fill the window.
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(scheme.panel))
            .show(ui, |ui| self.mail_list(ui));
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
/// itself out with it, so the same rhythm runs from the first cell to the
/// close button.
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
/// `titlebar_search_width` plans in. Fixed rather than filling: the box is
/// anchored where the message list starts (`titlebar_anchors`), and a box that
/// widened with the window would read as part of the columns rather than as the
/// list's own field.
const TITLEBAR_SEARCH_WIDTH: f32 = 175.0;
/// The narrowest the box is allowed to get before the bar gives it up
/// altogether (`titlebar_search_width`): below this it stops reading as a
/// field, and the search popup is still a Ctrl+F away. The margins come out of
/// this width like anything else, so the query is left with most of it.
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

/// The clear chrome the bar's anchored groups keep around themselves: between
/// the first group and the search box, between the box and the mail actions, and
/// between either of them and the drag surface. Not elastic — the groups are
/// placed at their columns' own edges, so this is the same gap however wide the
/// window is, and what gives way when the room runs out is the search box's
/// width and then the box itself.
const TITLEBAR_GROUP_GAP: f32 = 8.0;
/// How close the mail actions may come to the window controls. The run is
/// anchored to the reading pane; when the pane is wide enough to hold it beside
/// the controls this is the clearance it keeps, and when it is not the run steps
/// left over the pane's own edge instead — and no further than
/// `MAIL_ACTION_OVERHANG`.
const MAIL_ACTION_EDGE_GAP: f32 = 6.0;
/// How far the mail actions may sit over the reading pane's own edge before the
/// bar gives them up: the pane's card is inset from that edge by
/// `CONTENT_GUTTER` plus `CONTENT_PADDING.x`, so a run that hangs over
/// it is still over the pane's card and covers only the list's own margin. Past
/// that the run is not this pane's any more, and the mail's own card head —
/// which carries the same cells — is where the actions are.
const MAIL_ACTION_OVERHANG: f32 = CONTENT_GUTTER + CONTENT_PADDING.x;
/// How long the mail actions take to fade in and out with the mail they act on.
/// The bar's own state-change time (`TITLEBAR_FLOAT_TIME`), because it is the
/// same kind of change: what the bar shows following what the app has.
const MAIL_ACTION_FADE_TIME: f32 = 0.16;
/// The radius of the dot the sync cell carries while an account's last sync
/// stands failed, and the size of the spinner it becomes while one is running:
/// the cell is where a sweep is asked for, so it is where one is shown.
const SYNC_BADGE_RADIUS: f32 = 3.0;
const SYNC_SPINNER_SIZE: f32 = 20.0;
/// How large the mail actions' glyphs are drawn. Their outlines are designed in
/// a 24×24 box, which `glyph_point` scales to this.
const MAIL_ACTION_GLYPH: f32 = 18.0;

/// The mail card's own action cells and the glyphs in them (`mail_sheet`'s
/// head row): the title bar's run in a smaller square — `TITLEBAR_BUTTON_GAP`
/// between the cells, as in the bar, and `titlebar_hover`'s fill under the
/// pointer, since it is the same kind of cell on another surface. Smaller
/// because on a card the four share their row with the sender, where in the
/// bar they share theirs with the window's own controls.
const CARD_ACTION_SIZE: f32 = 26.0;
const CARD_ACTION_GLYPH: f32 = 15.0;

/// What the navigation column's label group keeps clear of the account above
/// it: the folders of an account end with a gap of their own, and this is what
/// makes the group read as its own thing under all of them.
const LABEL_GROUP_GAP: f32 = 10.0;

/// The label popup (the label cell's menu): as wide as its checklist and its
/// new-label box want, and no taller than this before the labels scroll — a
/// user with fifty labels should not get a menu taller than the window.
const LABEL_POPUP_WIDTH: f32 = 220.0;
/// The bar's two menus — the filter's and the sort's: a row per choice, each the
/// choice's own mark, its name, and the tick at the end that says whether it is
/// the one in force. One width for both, because they hang from adjacent cells
/// and are read as one control.
const LIST_MENU_WIDTH: f32 = 240.0;
/// The room a menu row takes, the gap between two of them, and how far the row's
/// content stays clear of its own ends — the mark's inset and the tick's, which
/// are one number so the two line up down the menu.
const FILTER_ROW_HEIGHT: f32 = 26.0;
const FILTER_ROW_GAP: f32 = 1.0;
const FILTER_ROW_INSET: f32 = 8.0;
/// The mark at a row's left, the room between it and the name, and the tick at
/// the row's end.
const FILTER_ICON_SIZE: f32 = 16.0;
const FILTER_ICON_GAP: f32 = 10.0;
const FILTER_TICK_SIZE: f32 = 14.0;
/// The hover fill a row takes under the pointer, rounded like the bar's cells.
const FILTER_ROW_CORNER_RADIUS: u8 = 4;
const LABEL_POPUP_MAX_HEIGHT: f32 = 240.0;
const LABEL_POPUP_FIELD_WIDTH: f32 = 120.0;

/// The labels a mail carries, drawn on its own card under the head: one chip per
/// label, as wide as the name wants, wrapping when the run does not fit the
/// card. The fill is the scheme's hover — the palette's faintest surface that is
/// not the card itself — and the text weak, because the labels are a note about
/// the mail rather than part of it. The corner is half the height, so a chip
/// reads as a pill rather than as a small button.
const LABEL_CHIP_HEIGHT: f32 = 18.0;
const LABEL_CHIP_PADDING: f32 = 6.0;
const LABEL_CHIP_GAP: f32 = 4.0;
const LABEL_CHIP_CORNER_RADIUS: u8 = 9;
/// What the chip run keeps clear of the head row above it.
const LABEL_CHIPS_TOP_GAP: f32 = 6.0;

/// How long the line above the message list stays there: long enough to read
/// where a downloaded file went — or that a mail went out — and act on it, short
/// enough not to become furniture. Whatever it says does not change when the line
/// goes.
const NOTICE_TIME: Duration = Duration::from_secs(8);

/// The automatic sync (Preferences → General): how many minutes apart its sweeps
/// are unless the preference says otherwise, and the range the preference
/// offers. A minute is the shortest that is not a busy loop, and a day the
/// longest that still reads as "automatically" rather than as a schedule.
const AUTO_SYNC_MINUTES: u32 = 15;
const AUTO_SYNC_MIN_MINUTES: u32 = 1;
const AUTO_SYNC_MAX_MINUTES: u32 = 1440;
const SECS_PER_MINUTE: u64 = 60;

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

/// The rows' padding inside the list's scroll area: the sides (`LIST_MARGIN`,
/// which keeps the mails off the hairlines the card is framed by) and the top.
/// The top is the title bar's height and it is *inside* the scroll area on
/// purpose: the first mail starts below the bar at rest, and scrolling takes
/// the padding away with the rows, which is what slides them under the floating
/// bar. It belongs to the scroll area rather than to the column, which is what
/// lets the scroll bar reach the column's edge. **There is no bottom padding**:
/// the list's card ends with its last mail (`list_card`), so a padding below
/// the rows would be a strip of surface under the last mail — and, scrolled to
/// the end, a strip of nothing under it.
const LIST_PADDING: egui::Margin = egui::Margin {
    left: LIST_MARGIN,
    right: LIST_MARGIN,
    top: TITLEBAR_HEIGHT_I8,
    bottom: 0,
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
/// The room between two cards of a conversation in the reading pane, which is
/// the chrome showing between them: a conversation is one card per mail
/// (`mail_card`), each of them the card a mail on its own gets.
const CONVERSATION_CARD_GAP: f32 = 12.0;

/// The three columns' minimum widths, and the two the splitters start
/// them at. The navigation and the reading pane are resizable panels, so
/// theirs are the panels' own bounds; the message list is what they leave,
/// so its minimum is one the app enforces itself (`plan_columns caps the
/// pane at what leaves the list this much) and what a window narrower than
/// it turns into a horizontal scrollbar instead (`mail_list).
const NAV_MIN_WIDTH: f32 = 150.0;
const NAV_DEFAULT_WIDTH: f32 = 200.0;
const CONTENT_MIN_WIDTH: f32 = 280.0;
const CONTENT_DEFAULT_WIDTH: f32 = 460.0;
const LIST_MIN_WIDTH: f32 = 240.0;

/// The popup pane's back button: a round, chevron-only cell floating over
/// the mail (see `mail_content`) — its diameter, how far it keeps from the
/// window's edge under the title bar, and the chevron it carries with the
/// stroke that chevron draws with.
const POPUP_BACK_DIAMETER: f32 = 36.0;
const POPUP_BACK_MARGIN: f32 = 10.0;
const POPUP_BACK_CHEVRON: f32 = 12.0;
const POPUP_BACK_STROKE: f32 = 1.5;

/// What the three columns become at one window width — the plan a frame's
/// layout is drawn from (`KimoApp::ui`). The columns give way to a
/// narrowing window in one order, so that whatever it loses is the least
/// useful thing it can spare: the navigation collapses first, then the
/// reading pane is capped at what leaves the list its minimum, then the pane
/// stops being a column at all and becomes a popup over the list, and the
/// list itself never gets narrower than its rows can read — it scrolls
/// sideways instead.
///
/// `nav` and `pane` are the widths to remember the two resizable columns
/// by, what their splitters left them at; `nav_collapsed` says whether the
/// navigation is taking room right now. The list is the remainder, so the
/// one minimum it has is enforced here rather than by a panel of its own.
struct ColumnPlan {
    /// The reading pane is a popup over the list rather than a column beside
    /// it: not even the two minimums fit side by side. A click on a mail
    /// opens it, its back button closes it (`mail_content`).
    popup: bool,
    /// The widest the reading pane may be this frame: `None` when it keeps
    /// its remembered width, and the room beside the list's minimum when the
    /// window has less than that for it.
    pane_max: Option<f32>,
    /// The window is narrower than the list's minimum: the list keeps the
    /// minimum and scrolls sideways rather than crushing its rows.
    list_scrolls: bool,
    /// The window has less than the three columns need with the list at its
    /// minimum: the navigation gives way. This is the crossing `KimoApp::ui`
    /// collapses it on — re-expanding stays a manual choice until the next
    /// one.
    collapses_nav: bool,
}

fn plan_columns(window: f32, nav: f32, pane: f32, nav_collapsed: bool) -> ColumnPlan {
    // What the two content columns share once the navigation has had its
    // width — none of it, when the navigation is collapsed.
    let room = window - if nav_collapsed { 0.0 } else { nav };
    // The pane's share is what is left beside the list's minimum, which is
    // also the one place that minimum is enforced: the list is the remainder,
    // and the pane is what the plan holds here.
    let pane_room = room - LIST_MIN_WIDTH;
    let popup = pane_room < CONTENT_MIN_WIDTH;
    ColumnPlan {
        popup,
        pane_max: (!popup && pane_room < pane).then_some(pane_room),
        list_scrolls: window < LIST_MIN_WIDTH,
        collapses_nav: window < nav + pane + LIST_MIN_WIDTH,
    }
}

/// A resizable panel's remembered width — what its splitter left it at, as
/// egui kept it from the previous frame. A collapsed navigation keeps its
/// own, because a panel that is not shown stores nothing.
fn panel_width(ctx: &egui::Context, panel: &str) -> Option<f32> {
    egui::PanelState::load(ctx, egui::Id::new(panel)).map(|state| state.outer_rect.width())
}

/// Place a panel at a width of the app's choosing: what a capped pane is
/// held at while the cap lasts, and what it is brought back to once the cap
/// lifts (`KimoApp::ui`) — egui on its own would only ever remember what a
/// panel last rendered at. Only the size is meant; the position is a dummy.
fn set_panel_width(ctx: &egui::Context, panel: &str, width: f32) {
    let rect = egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(width, ctx.viewport_rect().height()),
    );
    ctx.data_mut(|data| {
        data.insert_persisted(egui::Id::new(panel), egui::PanelState { outer_rect: rect })
    });
}
/// The attachment cards under a mail's body: one card per file, as wide as the
/// name and the type it carries want — never narrower than `ATTACHMENT_CARD_MIN_WIDTH`
/// and never wider than the mail's own column — and the height of a line of the
/// list. They are laid across that column and wrapped onto the next row when the
/// next card does not fit, `ATTACHMENT_CARD_GAP` between two of them either
/// way, and `ATTACHMENT_CARDS_TOP_GAP` between the run and the body above it.
/// The card's own paddings are the glyph's inset, its size, the gap it leaves
/// the text and the text's own right inset; only a name the column itself cannot
/// hold is cut, and the tooltip then shows it.
const ATTACHMENT_CARD_MIN_WIDTH: f32 = 140.0;
const ATTACHMENT_CARD_HEIGHT: f32 = 52.0;
/// The card's two lines, in points: the file name and, under it, the type and
/// the size. Both are smaller than the body text — a card is a note about the
/// mail, not part of it.
const ATTACHMENT_NAME_FONT_SIZE: f32 = 11.0;
const ATTACHMENT_META_FONT_SIZE: f32 = 9.0;
const ATTACHMENT_CARD_GAP: f32 = 8.0;
const ATTACHMENT_CARDS_TOP_GAP: f32 = 18.0;
const ATTACHMENT_CARD_CORNER_RADIUS: u8 = 6;
/// The cross a compose card's file is taken off with: a circle of this size, its
/// own corner radius (half of it — a circle), and the round cross glyph inside.
const ATTACHMENT_CLOSE_SIZE: f32 = 18.0;
const ATTACHMENT_CLOSE_RADIUS: u8 = 9;
const ATTACHMENT_CARD_PADDING: f32 = 12.0;
const ATTACHMENT_GLYPH: f32 = 22.0;
const ATTACHMENT_LINE_GAP: f32 = 2.0;
/// Room the magnifier takes inside the search box, and the glyph's size. The
/// inset is the field's left margin, so the text begins after the icon.
const SEARCH_ICON_INSET: i8 = 22;
const SEARCH_ICON_SIZE: f32 = 15.0;
/// How far the title bar's first cell starts in from the window's left edge —
/// the space added before it, as the left group's own first item. Wider than
/// the margin at the other end, which the window controls keep.
const TITLEBAR_LEFT_INSET: f32 = 16.0;
/// How far the window controls end short of the window's own right edge, so the
/// close button's rounded fill is not flush against the frame — flush, it
/// reaches the window's corner.
const TITLEBAR_RIGHT_MARGIN: f32 = 10.0;

/// The compose modal: the width of its card (what the boxes are laid out for),
/// the label column of its header fields, the height of one of those one-line
/// fields, and the height of the body box, which scrolls rather than growing
/// the card.
const COMPOSE_WIDTH: f32 = 560.0;
const COMPOSE_LABEL_WIDTH: f32 = 56.0;
/// The footer buttons the on-the-spot card draws: an icon and a word in one
/// target, taller than a stock button and wider than its label needs. The
/// popup keeps egui's own two — its form is wide and its footer is a line of it,
/// while this card is a card and its two buttons are what it is for.
const COMPOSE_BUTTON_HEIGHT: f32 = 34.0;
const COMPOSE_BUTTON_PADDING: f32 = 12.0;
const COMPOSE_BUTTON_ICON: f32 = 16.0;
const COMPOSE_BUTTON_ICON_GAP: f32 = 8.0;
const COMPOSE_BUTTON_FONT_SIZE: f32 = 13.0;
const COMPOSE_BUTTON_CORNER_RADIUS: u8 = 6;
const COMPOSE_FIELD_HEIGHT: f32 = 22.0;
const COMPOSE_BODY_HEIGHT: f32 = 320.0;
/// The cell the reply's To row ends with on the spot: the chevron that folds
/// the Cc and Bcc lines away, and the square the pointer clicks it in — a
/// field's own height, so the row keeps its height whether they are out or
/// away — and the chevron drawn inside that cell.
const COMPOSE_FOLD_CELL: f32 = COMPOSE_FIELD_HEIGHT;
const COMPOSE_FOLD_GLYPH: f32 = 12.0;
/// The area the compose card's attach target takes: one line of text, the width
/// of the field column, and the corner radius the cards under it use.
const ATTACH_CARD_HEIGHT: f32 = 46.0;
const ATTACH_CARD_CORNER_RADIUS: u8 = 6;
const ATTACH_CARD_TEXT_SIZE: f32 = 13.0;

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
    let controls_left = bar.right() - TITLEBAR_RIGHT_MARGIN - titlebar_cells_width(controls);
    let cog_left = controls_left - TITLEBAR_BUTTON_GAP - TITLEBAR_BUTTON_SIZE;
    let sync_left = cog_left - TITLEBAR_BUTTON_GAP - TITLEBAR_BUTTON_SIZE;
    TitlebarRects {
        content: egui::Rect::from_min_max(
            bar.min,
            egui::pos2(sync_left.clamp(bar.left(), bar.right()), bar.bottom()),
        ),
        sync: cell(sync_left, TITLEBAR_BUTTON_SIZE),
        cog: cell(cog_left, TITLEBAR_BUTTON_SIZE),
        controls: cell(
            controls_left,
            bar.right() - TITLEBAR_RIGHT_MARGIN - controls_left,
        ),
    }
}

/// The width the bar gives the search box in the room that is left for it: its
/// own width while there is room, as narrow as its floor while the room is
/// short, and nothing at all below that. The box is the one thing the bar gives
/// up — the first group never gives way, and the mail actions are the pane's own
/// row rather than the room's leftovers — and the search popup stays a Ctrl+F
/// away when it is gone.
///
/// Monotone in `room`, which is what keeps a window being resized from
/// flickering between two shapes. Nothing here is a rect: the caller draws the
/// box inside the room it was given and clips it to that room.
fn titlebar_search_width(room: f32) -> Option<f32> {
    if room >= TITLEBAR_SEARCH_WIDTH {
        Some(TITLEBAR_SEARCH_WIDTH)
    } else if room >= TITLEBAR_SEARCH_FLOOR {
        Some(room)
    } else {
        None
    }
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

/// Place the two groups the bar anchors to the columns below it: the search box
/// over the message list, the mail actions over the reading pane. Both edges are
/// the columns' own (`KimoApp::list_column`), so both follow the splitters that
/// move those columns, and both are clamped clear of the first group — the nav
/// toggle and the pen, which never give way because the compose button needs no
/// mail open.
///
/// The run is placed only where it belongs: at the pane's edge, or over it by no
/// more than `MAIL_ACTION_OVERHANG` when the window controls leave no room
/// there, and never within `MAIL_ACTION_EDGE_GAP` of those controls. A pane
/// too narrow to hold the run beside them gives it up, and the mail's own card
/// head — the same cells, on the mail itself — is where the actions are then.
fn titlebar_anchors(
    bar: egui::Rect,
    first_end: f32,
    list_left: f32,
    pane_left: f32,
    cluster_left: f32,
) -> TitlebarAnchors {
    let width = titlebar_cells_width(MailAction::ALL.len());
    // The pane's edge, or as far left of it as the window controls leave — and
    // in the popup layout, where the pane is the whole window, the first group's
    // own end.
    let preferred = pane_left.max(first_end + TITLEBAR_GROUP_GAP);
    let left = preferred.min(cluster_left - MAIL_ACTION_EDGE_GAP - width);
    let fits = left >= first_end + TITLEBAR_GROUP_GAP
        && left + width + MAIL_ACTION_EDGE_GAP <= cluster_left + 0.5
        // The pane's own run while the pane is a column of its own: one that
        // hangs further over the pane's edge than the card's margin is over the
        // list, and no longer this pane's. A popup pane is the whole window, so
        // there its edge is behind the first group and there is nothing to hang
        // over.
        && (pane_left < first_end + TITLEBAR_GROUP_GAP || left >= pane_left - MAIL_ACTION_OVERHANG);
    let actions = fits.then(|| {
        egui::Rect::from_min_size(
            egui::pos2(left, bar.center().y - TITLEBAR_BUTTON_SIZE / 2.0),
            egui::vec2(width, TITLEBAR_BUTTON_SIZE),
        )
    });
    // The box's own width is planned first, exactly as it was before there were
    // cells to sit beside it: the box is the list's, and a bar that cannot hold
    // everything holds the box. What the box leaves is the cells' room — they are
    // one group with it, a cell's gap apart — and each cell is drawn while the
    // room reaches its own end: the filter first, then the sort, so a bar that
    // can hold one of the two holds the filter. The box's width never depends on
    // them, which is what keeps a window being resized from flickering.
    let search = list_left.max(first_end + TITLEBAR_GROUP_GAP);
    let next = actions.map_or(cluster_left, |actions| actions.left());
    let search_width = titlebar_search_width(next - search - TITLEBAR_GROUP_GAP);
    let mut cell_left = search + search_width.unwrap_or_default() + TITLEBAR_BUTTON_GAP;
    let room_for_cell = |left: f32| left + TITLEBAR_BUTTON_SIZE + TITLEBAR_GROUP_GAP <= next + 0.5;
    let cell = |left: f32| {
        egui::Rect::from_min_size(
            egui::pos2(left, bar.center().y - TITLEBAR_BUTTON_SIZE / 2.0),
            egui::vec2(TITLEBAR_BUTTON_SIZE, TITLEBAR_BUTTON_SIZE),
        )
    };
    let mut next_cell = || {
        let rect = room_for_cell(cell_left).then(|| cell(cell_left));
        if rect.is_some() {
            cell_left += TITLEBAR_BUTTON_SIZE + TITLEBAR_BUTTON_GAP;
        }
        rect
    };
    let filter = search_width.and_then(|_| next_cell());
    let sort = filter.and_then(|_| next_cell());
    TitlebarAnchors {
        search,
        search_width,
        filter,
        sort,
        actions,
    }
}

/// The width of a run of `count` cells in the title bar: `TITLEBAR_BUTTON_SIZE`
/// squares, `TITLEBAR_BUTTON_GAP` apart. Every run is measured with this — the
/// mail actions (which are placed at the reading pane's own edge), the window
/// controls and the cells the sync and the cog are carved as — so no run can
/// disagree with the cells it draws.
fn titlebar_cells_width(count: usize) -> f32 {
    count as f32 * TITLEBAR_BUTTON_SIZE + count.saturating_sub(1) as f32 * TITLEBAR_BUTTON_GAP
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

/// The chrome showing between two cards of the reading pane, a mail's and
/// the compose's alike: the room `add_space` is asked for is the room wanted
/// minus the item spacing it adds on top of what it is given.
fn pane_card_gap(ui: &egui::Ui) -> f32 {
    (CONVERSATION_CARD_GAP - ui.spacing().item_spacing.y).max(0.0)
}

/// One button in the title bar: a `TITLEBAR_BUTTON_SIZE` square, centred in the
/// bar's height by the row that lays it out, empty at rest and filled with a
/// rounded corner under the pointer (`titlebar_hover`). Every button in the bar
/// is this — the nav toggle, compose, sync, the mail actions, the cog and the
/// window controls — and `enabled` is the one thing a caller varies: the mail
/// actions' disabled cells neither sense clicks nor fill, while the rest of the
/// bar is always live.
fn titlebar_button(ui: &mut egui::Ui, enabled: bool) -> egui::Response {
    titlebar_button_faded(ui, enabled, 1.0)
}

/// The same cell, drawn at `fade` of its strength — the bar's own cell, for the
/// one run that has a state to come and go with (`titlebar_mail_actions`).
fn titlebar_button_faded(ui: &mut egui::Ui, enabled: bool, fade: f32) -> egui::Response {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (_, response) = ui.allocate_exact_size(egui::Vec2::splat(TITLEBAR_BUTTON_SIZE), sense);
    if enabled {
        titlebar_hover_faded(ui, response, fade)
    } else {
        response
    }
}

/// The color a mail card's action glyph is drawn in: the scheme's weak text at
/// rest, the normal text color under the pointer. On a card the four glyphs
/// sit on the mail's own head, where the text color would make them the
/// loudest thing on it; the pointer is what says which one is being read, and
/// the hover fill is under it by then. A list row's own delete button draws
/// the same way (`text_weak` until the pointer finds it).
fn card_action_color(scheme: ColorScheme, hovered: bool) -> egui::Color32 {
    if hovered {
        scheme.text
    } else {
        scheme.text_weak
    }
}

/// One of a mail card's own action cells (`mail_sheet`'s head row): the
/// title bar's button in a smaller square — `CARD_ACTION_SIZE` against
/// `TITLEBAR_BUTTON_SIZE` — with the same rest look and the same hover fill,
/// the glyph being the caller's either way.
fn card_action_button(ui: &mut egui::Ui, enabled: bool) -> egui::Response {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (_, response) = ui.allocate_exact_size(egui::Vec2::splat(CARD_ACTION_SIZE), sense);
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
    titlebar_hover_faded(ui, response, 1.0)
}

/// The same fill at `fade` of its strength, for a cell that is fading in or
/// out with the thing it acts on (`titlebar_mail_actions`): the fill and the
/// glyph have to come and go together, or the pointer's cell would flash.
fn titlebar_hover_faded(ui: &egui::Ui, response: egui::Response, fade: f32) -> egui::Response {
    if response.hovered() {
        titlebar_fill(
            ui,
            response.rect,
            ui.visuals().widgets.hovered.bg_fill.gamma_multiply(fade),
        );
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
/// The paper a Blitz page is drawn on when the mail declares no background of its
/// own and the reader has switched this mail away from white: the dark scheme's
/// content surface, so a dark paper is the surface the app draws mail on
/// elsewhere rather than a colour of its own.
const BLITZ_PAPER_DARK: egui::Color32 = crate::theme::DARK.content;

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
/// The card the message list sits on: the column's own surface, as tall as the
/// list's content — the rows and the list's padding — and never taller than the
/// column, with a hairline down each side. Those are the mails' own left and
/// right edges: they run the card's whole height so the column stays framed
/// above and below the rows, and every row paints them again over its own fill
/// (see `message_card`).
///
/// `height` is what the list reported; `None` — and a height of nothing —
/// leaves the shape a `Noop`, which is the bare chrome a placeholder stands on.
fn list_card(
    column: egui::Rect,
    height: Option<f32>,
    scheme: crate::theme::ColorScheme,
) -> egui::Shape {
    let Some(height) = height.filter(|height| *height > 0.0) else {
        return egui::Shape::Noop;
    };
    let card = egui::Rect::from_x_y_ranges(
        column.x_range(),
        column.top()..=(column.top() + height).min(column.bottom()),
    );
    let stroke = egui::Stroke::new(1.0_f32, scheme.row_edge);
    egui::Shape::Vec(vec![
        egui::Shape::rect_filled(card, 0.0, scheme.content),
        egui::Shape::vline(card.left() + 0.5, card.y_range(), stroke),
        egui::Shape::vline(card.right() - 0.5, card.y_range(), stroke),
    ])
}

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
    // The rows span the column — and, when the window is narrower than the
    // list's minimum, the minimum the list keeps for itself (`set_min_width`
    // in `mail_list`): the union, so a list that scrolls sideways lays its
    // rows out at the width they scroll over rather than at the viewport's.
    let xs = ui.max_rect().union(ui.min_rect()).x_range();
    let rect = egui::Rect::from_x_y_ranges(xs, top..=bottom);
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |rows_ui| {
        // Keep the widgets' auto ids stable as the list scrolls.
        rows_ui.skip_ahead_auto_ids(first);
        add_contents(rows_ui, first..last)
    })
    .inner
}

/// How long between automatic syncs, from the preference's minutes: the app's
/// own default when settings.json names none, and clamped to the range the
/// preferences page offers however the file was edited.
fn auto_sync_interval(minutes: Option<u32>) -> Duration {
    let minutes = minutes
        .unwrap_or(AUTO_SYNC_MINUTES)
        .clamp(AUTO_SYNC_MIN_MINUTES, AUTO_SYNC_MAX_MINUTES);
    Duration::from_secs(minutes as u64 * SECS_PER_MINUTE)
}

/// How long until the automatic sync is next due: `None` when it is not running
/// at all — switched off in Preferences — and zero from the moment the interval
/// since the last sweep has passed, however long ago that was.
fn auto_sync_wait(enabled: bool, last: Instant, every: Duration, now: Instant) -> Option<Duration> {
    enabled.then(|| every.saturating_sub(now.saturating_duration_since(last)))
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
    let response = ui.add(Field::new(
        egui::TextEdit::singleline(value)
            .hint_text(hint)
            .desired_width(300.0),
    ));
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
///
/// The page is drawn on `paper`: a mail that declares an html or body
/// background has it painted across the whole page by the renderer's own canvas
/// fill (`blitz-paint`'s `paint_scene`), which covers what is drawn here, so
/// this shows only where the mail painted nothing at all — which, for a mail
/// written for white paper, is most of it.
fn draw_blitz_page(ui: &mut egui::Ui, page: &RasterizedPage, paper: egui::Color32) {
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
    // Under the tiles, because a painter draws in the order it is given: the
    // paper is the page box, scaled with the page like the tiles are.
    ui.painter().rect_filled(
        egui::Rect::from_min_size(origin, page.size * fit),
        0.0,
        paper,
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

const MESSAGE_ROW_HEIGHT: f32 = 68.0;
/// The select-all row at the top of the list in select mode: its own height,
/// not a mail row's — it is one line of chrome with a box on it — as the
/// frame margin wants it (whole points, like `LIST_PADDING`) and as the
/// drawing wants it.
const SELECT_ALL_HEIGHT_I8: i8 = 32;
const SELECT_ALL_HEIGHT: f32 = SELECT_ALL_HEIGHT_I8 as f32;
const AVATAR_DIAMETER: f32 = 32.0;
/// The room between an avatar and the name beside it — the message list's
/// own gap, kept by the pane's sender row so the two read alike.
const AVATAR_TEXT_GAP: f32 = 10.0;
/// The checkbox a row draws where its avatar would be — select mode's control,
/// and the way into the mode — with the room its click target keeps around the
/// box on every side, which is also the rounded fill it shows under the
/// pointer. The box is not the avatar's size: it is a control, and an 18 pt box
/// in a 32 pt slot reads as one, while a 32 pt box would read as a button.
const CHECKBOX_SIZE: f32 = 18.0;
const CHECKBOX_PADDING: f32 = 5.0;
const CHECKBOX_CORNER_RADIUS: u8 = 4;
/// The delete button a mail row offers under the pointer: the cell at the right
/// end of its first line — where the timestamp sits, which gives way to it —
/// and the glyph drawn in it. Smaller than the title bar's cells: a row's
/// control is a note on the mail, not chrome.
const ROW_ACTION_SIZE: f32 = 24.0;
const ROW_ACTION_GLYPH: f32 = 15.0;
const ROW_ACTION_CORNER_RADIUS: u8 = 4;
/// The gap between a row's own cells — the flag and the delete — which is the
/// title bar's own gap: the pair is one run by the row's edge, the way the bar's
/// run and the mail head's are.
const ROW_ACTION_GAP: f32 = 6.0;
/// The width of the check drawn on a checked box, and of the dash a partly
/// checked conversation gets.
const CHECKBOX_STROKE: f32 = 2.0;
/// Font size of the avatar's initial — a bit under half the diameter.
const AVATAR_FONT_SIZE: f32 = 14.0;
/// Font sizes of the row's three lines, in points: the timestamp, the title
/// line (sender over subject, the one that carries the unread weight) and the
/// muted preview under it.
const ROW_DATE_FONT_SIZE: f32 = 11.0;
const ROW_TITLE_FONT_SIZE: f32 = 12.0;
const ROW_PREVIEW_FONT_SIZE: f32 = 11.0;
/// Where those three lines are centred, measured down from the row's top edge:
/// the sender and the timestamp, the subject under it, and the preview under
/// that. They are one block centred in the row — the subject line is the one
/// straddling the middle of `MESSAGE_ROW_HEIGHT` — which is what leaves the room
/// above the sender's line and the room below the preview the same; spacing the
/// lines from the row's top edge left the preview twice as close to the bottom
/// as the sender was to the top.
const ROW_SENDER_Y: f32 = 15.0;
const ROW_SUBJECT_Y: f32 = 35.0;
const ROW_PREVIEW_Y: f32 = 52.0;
/// How far a conversation's own row — and the mails it holds — are inset from
/// the column's edge, and the room the chevron is drawn in at the left of that.
/// The two are one number: what the chevron is drawn in is exactly what the
/// thread's mails are inset by, so the block reads as one thing. Every row is
/// inset by it, a mail with nothing to open included, so the avatars line up
/// down the whole list.
const THREAD_INDENT: f32 = 18.0;
const THREAD_CHEVRON: f32 = 10.0;
/// How much further in a mail of an *open* conversation is drawn, on top of
/// `THREAD_INDENT`, and the avatar it is drawn with: a step and a smaller
/// circle instead of the guide line such a mail used to draw down the chevron's
/// column. The circle starts exactly `THREAD_STEP` further in, and the mail's
/// text `THREAD_STEP` + `THREAD_AVATAR_DIAMETER` - `AVATAR_DIAMETER` — the
/// run between the two circles, which is 12 pt: the smaller avatar gives part of
/// the step back. The step is the whole of the mark, so it has to be visible
/// without reading as a misalignment.
const THREAD_STEP: f32 = 20.0;
const THREAD_AVATAR_DIAMETER: f32 = 24.0;
/// Font size of that avatar's initial — a bit under half the diameter.
const THREAD_AVATAR_FONT_SIZE: f32 = 11.0;
/// How much of the row's own weak color the chevron keeps at rest. It is a
/// quiet control at the edge of a row, and it comes up to the full color under
/// the pointer, which is what says it can be clicked.
const THREAD_CHEVRON_ALPHA: f32 = 0.65;
/// The paperclip a row draws when the mail has parts: the box its outline is
/// drawn in (`paint_clip_icon` maps the glyph's 24-unit design onto it), the
/// width the clip actually comes out at — a clip is taller than it is wide, so
/// the box is square and the drawn glyph is not — and the room it keeps in
/// front of itself. The title line gives up the width and that room; the box's
/// own height is what keeps the glyph on the line.
const ROW_CLIP_SIZE: f32 = 14.0;
const ROW_CLIP_WIDTH: f32 = 8.5;
const ROW_CLIP_GAP: f32 = 7.0;
/// The flag a marked mail draws at the end of its title line, in the slot the
/// paperclip shares with it. The size is the box the 24×24 outline is mapped
/// into, and the width is what that outline comes to at it — its pole and
/// banner span x 5 to 19 — which is the room the title leaves for it.
const ROW_FLAG_SIZE: f32 = 14.0;
const ROW_FLAG_WIDTH: f32 = (19.0 - 5.0) / 24.0 * ROW_FLAG_SIZE;

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

/// One attachment as a card: the file's glyph and its name — a link, which
/// downloads the file when it is clicked (`KimoApp::download_attachment`) —
/// with the type and the size under the name for mail that was fetched with
/// them.
///
/// The card is as wide as the name and type it carries want, so a run of them
/// reads as the parts of one mail rather than as a column of its own, and the
/// *whole card* is the link rather than the points of the name: the name is
/// drawn in the
/// scheme's link color the way an HTML link is, the card fills with the hover
/// color while the pointer is on it, the name is underlined then, and the
/// pointer becomes a hand. Those are the four things that say a target is
/// there — and it is a real one now: the bytes are on the server, and a click
/// fetches them (see the attachment-card convention in AGENTS.md). The hover
/// text is what shows a name the column had to cut.
/// The compose card's own way to a file: one area the width of the field column
/// that says what it does. A click on it opens the desktop's dialog
/// (`crate::portal`), and files dragged from a file manager land on the *window*
/// and are attached by `KimoApp::attach_dropped` — so what this draws is one
/// target for both, rather than the button and the path box it used to take to
/// say the same thing.
///
/// `ready` is the state the whole card is drawn from: files are over the window,
/// or the dialog is open, and the card says so in the accent colour rather than
/// only in its text.
fn attach_card(
    ui: &mut egui::Ui,
    width: f32,
    text: &str,
    ready: bool,
    enabled: bool,
) -> egui::Response {
    let scheme = theme::of(ui.ctx());
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, ATTACH_CARD_HEIGHT), egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let hovered = response.hovered();
    let stroke = if ready || (hovered && enabled) {
        scheme.accent
    } else {
        scheme.border
    };
    ui.painter().rect_filled(
        rect,
        ATTACH_CARD_CORNER_RADIUS,
        if ready || (hovered && enabled) {
            scheme.hover
        } else {
            scheme.card
        },
    );
    ui.painter().rect_stroke(
        rect,
        ATTACH_CARD_CORNER_RADIUS,
        egui::Stroke::new(1.0_f32, stroke),
        egui::StrokeKind::Inside,
    );
    let color = if ready {
        scheme.accent
    } else {
        scheme.text_weak
    };
    let font = egui::FontId::proportional(ATTACH_CARD_TEXT_SIZE);
    let galley = ui
        .ctx()
        .fonts_mut(|fonts| fonts.layout_no_wrap(text.to_string(), font, color));
    let at = rect.center() - galley.size() / 2.0;
    ui.painter().galley(at, galley, color);
    if enabled {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        response.on_hover_text("Files dropped anywhere on the window are attached to this mail")
    } else {
        response
    }
}

/// The compose card's files: what would go out with the mail, drawn as the
/// reading pane draws it — the same card, because a file is the same thing in
/// both places — and the one area that adds another, through the desktop's own
/// dialog (`attach_card`). A drop on the window is the third way in, and the
/// window's rather than the card's (`KimoApp::ui`): on Wayland winit delivers
/// no drops at all, so what a drop does is decided where the whole window's
/// input is read. A click on a file's card takes it back off the mail.
///
/// Returns the file a click took off, which the caller applies once the card
/// is drawn — the run must not be changed under the cards that are reading it
/// — and whether the dialog was asked for.
fn compose_files_row(
    ui: &mut egui::Ui,
    draft: &ComposeDraft,
    width: f32,
    picking: bool,
    sending: bool,
) -> (Option<usize>, bool) {
    let label = |ui: &mut egui::Ui, text: &str| {
        ui.add_sized(
            [COMPOSE_LABEL_WIDTH, COMPOSE_FIELD_HEIGHT],
            egui::Label::new(text).halign(egui::Align::RIGHT),
        );
    };
    // While files are over the window the box says what a drop would do.
    let hovering = ui.ctx().input(|input| !input.raw.hovered_files.is_empty());
    let mut remove = None;
    let mut pick = false;
    ui.horizontal_top(|ui| {
        label(ui, "Files");
        ui.vertical(|ui| {
            ui.set_width(width);
            // The files as the reading pane draws them: the same card,
            // because a file is the same thing in both places — and a
            // click takes it off this mail, which is what the card's
            // own tooltip says here (`attachment_card`).
            if !draft.attachments.is_empty() {
                ui.spacing_mut().item_spacing = egui::Vec2::splat(ATTACHMENT_CARD_GAP);
                ui.horizontal_wrapped(|ui| {
                    for (index, file) in draft.attachments.iter().enumerate() {
                        let attachment = Attachment {
                            name: file.name.clone(),
                            content_type: file.content_type.to_string(),
                            size: file.size().unwrap_or(0),
                        };
                        let card = attachment_card(
                            ui,
                            &attachment,
                            width,
                            "Click to take it off this mail",
                            true,
                        );
                        let taken =
                            card.card.clicked() || card.close.is_some_and(|close| close.clicked());
                        if taken && !sending {
                            remove = Some(index);
                        }
                    }
                });
                ui.add_space(4.0);
            }
            // One area in place of the three controls this used to take — a
            // button, a path box and a second button — because the dialog is
            // the way in and the path box was only ever the fallback for a
            // session that has no dialog to open.
            let text = if picking {
                "Choosing files…"
            } else if hovering {
                "Drop the files here"
            } else {
                "Click to select files, or drag files here"
            };
            let area = attach_card(ui, width, text, hovering || picking, !sending);
            if area.clicked() && !sending && !picking {
                pick = true;
            }
            if let Some(error) = &draft.attach_error {
                ui.label(
                    egui::RichText::new(error)
                        .small()
                        .color(theme::of(ui.ctx()).danger),
                );
            }
        });
    });
    (remove, pick)
}

/// The compose card's body: the writing area, `COMPOSE_BODY_HEIGHT` tall, that
/// scrolls on its own once the mail outgrows it, so the card never grows with
/// the draft. The box is that whole height and not the dozen lines an empty
/// draft happens to need (`min_size`): the room the card gives the writing is
/// the room the box takes, so the row under it — the files on the spot, the
/// footer in the window — is not read across a band of nothing.
///
/// **The box is the card's, drawn around the room rather than around the
/// text.** A text edit draws its own frame about what it holds; an answer
/// holds a mail with a quote under it, far more text than the
/// `COMPOSE_BODY_HEIGHT` viewport does, so a frame of the field's own would
/// run past the room at both ends and go where the scroll goes — its top out
/// of sight after a scroll down, its bottom before one, which reads as a box
/// with two sides. The frame is drawn here instead, in the viewport's own
/// rect: the fill under the writing and the line over it, the way every other
/// box in the app is drawn, with the field given no frame of its own
/// (`take_box_off`).
///
/// The area is claimed outright rather than left to what the pane has under the
/// row above: a window too short for the card is the *pane's* to scroll, and a
/// scroll area that took the room it was given would squeeze the writing area
/// to nothing there, with the box's own text hidden behind a viewport a few
/// lines tall.
///
/// `caret_at_end` is false when the card opens with the caret at the top of
/// what is there — an answer over its quoted original, where the first
/// keystroke starts the reply rather than appending under the quote. Returns
/// the field's response, which is what puts the keyboard in it on the card's
/// first frame.
fn compose_body(
    ui: &mut egui::Ui,
    body: &mut String,
    width: f32,
    sending: bool,
    caret_at_end: bool,
) -> egui::Response {
    let area = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(width, COMPOSE_BODY_HEIGHT));
    ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| {
        // The box's fill, under the writing: what a stock field sits on.
        let corner_radius = ui.visuals().widgets.inactive.corner_radius;
        ui.painter()
            .rect_filled(area, corner_radius, ui.visuals().text_edit_bg_color());
        let field = egui::ScrollArea::vertical()
            .id_salt("compose-body")
            .max_height(COMPOSE_BODY_HEIGHT)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_enabled(
                    !sending,
                    egui::TextEdit::multiline(body)
                        .frame(field_frame())
                        .hint_text("Write your message…")
                        .desired_width(width)
                        .min_size(egui::vec2(width, COMPOSE_BODY_HEIGHT))
                        .cursor_at_end(caret_at_end),
                )
            })
            .inner;
        // And its line, over the writing: the rule every field of the app
        // follows (`Field`) — the accent while the keyboard is in it, the
        // theme's own border while it waits — drawn on the room's rect rather
        // than on the text's, which is what keeps the box still.
        let visuals = ui.style().interact(&field);
        let stroke = if field.has_focus() {
            egui::Stroke::new(FIELD_STROKE, theme::of(ui.ctx()).accent)
        } else {
            visuals.bg_stroke
        };
        ui.painter()
            .rect_stroke(area, corner_radius, stroke, egui::StrokeKind::Inside);
        field
    })
    .inner
}

/// The send glyph: a paper plane — a dart pointing right, its two wings meeting
/// at the fold. Two convex fills rather than one path, because that is what the
/// tessellator wants and a notch is not convex.
fn paint_send_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let r = rect.width().min(rect.height()) / 2.0;
    let at = |x: f32, y: f32| egui::pos2(c.x + r * x, c.y + r * y);
    // A dart pointing right: the tip, the two back corners, and the fold
    // between them — indented to the *right* of the back edge, which is what
    // leaves two wings of equal area. (A fold left of that line flattens the
    // lower wing into a sliver, and the whole glyph then reads as a slash.)
    let tip = at(0.95, 0.0);
    let back_top = at(-0.75, -0.85);
    let notch = at(-0.3, 0.0);
    let back_bottom = at(-0.75, 0.85);
    p.add(egui::Shape::convex_polygon(
        vec![tip, back_top, notch],
        color,
        egui::Stroke::NONE,
    ));
    p.add(egui::Shape::convex_polygon(
        vec![tip, notch, back_bottom],
        color,
        egui::Stroke::NONE,
    ));
}

/// One of the compose footer's own buttons, where the card has the room for
/// them: an icon, a word, and tall enough to be the thing the form points at.
/// `primary` is the accent-filled one — Send, because it is what the card is
/// for; the other is a quiet outline that turns the scheme's danger on hover,
/// the treatment a delete button gets everywhere else in the app. A disabled
/// button is the card's fill and a muted word: what it would do is in its
/// tooltip (`on_disabled_hover_text`, the caller's).
fn compose_button(
    ui: &mut egui::Ui,
    label: &str,
    icon: impl Fn(&egui::Painter, egui::Rect, egui::Color32),
    primary: bool,
    enabled: bool,
) -> egui::Response {
    let scheme = theme::of(ui.ctx());
    let font = egui::FontId::proportional(COMPOSE_BUTTON_FONT_SIZE);
    let galley = ui
        .ctx()
        .fonts_mut(|fonts| fonts.layout_no_wrap(label.to_string(), font, scheme.text));
    let width = COMPOSE_BUTTON_PADDING * 2.0
        + COMPOSE_BUTTON_ICON
        + COMPOSE_BUTTON_ICON_GAP
        + galley.size().x;
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, COMPOSE_BUTTON_HEIGHT),
        egui::Sense::click(),
    );
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let hovered = response.hovered() && enabled;
    let (fill, stroke, ink) = match (enabled, primary) {
        (false, _) => (
            scheme.card,
            egui::Stroke::new(1.0_f32, scheme.border),
            scheme.text_weak,
        ),
        (true, true) => (
            if hovered {
                scheme.accent.lerp_to_gamma(egui::Color32::WHITE, 0.12)
            } else {
                scheme.accent
            },
            egui::Stroke::NONE,
            scheme.on_accent,
        ),
        (true, false) => (
            if hovered {
                scheme.danger_hover
            } else {
                scheme.card
            },
            egui::Stroke::new(1.0_f32, scheme.border),
            if hovered {
                scheme.on_danger
            } else {
                scheme.text
            },
        ),
    };
    ui.painter()
        .rect_filled(rect, COMPOSE_BUTTON_CORNER_RADIUS, fill);
    ui.painter().rect_stroke(
        rect,
        COMPOSE_BUTTON_CORNER_RADIUS,
        stroke,
        egui::StrokeKind::Inside,
    );
    let glyph = egui::Rect::from_center_size(
        egui::pos2(
            rect.left() + COMPOSE_BUTTON_PADDING + COMPOSE_BUTTON_ICON / 2.0,
            rect.center().y,
        ),
        egui::Vec2::splat(COMPOSE_BUTTON_ICON),
    );
    icon(ui.painter(), glyph, ink);
    ui.painter().galley(
        egui::pos2(
            glyph.right() + COMPOSE_BUTTON_ICON_GAP,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        ink,
    );
    if enabled {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// Take the box off the widgets of this `Ui`: no outline, no fill, so what is
/// drawn in it stands on the surface under it and the box — if the caller
/// wants one — is the caller's to paint.
///
/// A text box wears its box from two slots of its own rather than the widgets'
/// — the fill it sits on (`Visuals::text_edit_bg_color`) and, with the
/// keyboard in it, the stroke of the selection — so both are taken off here
/// too: a field whose box is drawn by hand is flat focused and unfocused
/// alike. A combo reads neither, so the From list loses nothing by it.
///
/// It is scoped to the `Ui` it is called on, so the rest of the form keeps its
/// boxes.
fn take_box_off(ui: &mut egui::Ui) {
    let visuals = ui.visuals_mut();
    visuals.text_edit_bg_color = Some(egui::Color32::TRANSPARENT);
    visuals.selection.stroke = egui::Stroke::NONE;
    let widgets = &mut visuals.widgets;
    for state in [
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        state.bg_stroke = egui::Stroke::NONE;
        state.weak_bg_fill = egui::Color32::TRANSPARENT;
    }
}

/// `take_box_off`, and the hover a flat control keeps: one drawn as a line of
/// text rather than as a box still says it can be used by filling under the
/// pointer — the one thing left of a widget's own look. The answer's From list
/// is drawn with it (`compose_card`); the fields that are flat wear
/// `Field::flat` instead, which takes their box off without touching the
/// colour of the text selected inside them.
fn flatten_widgets(ui: &mut egui::Ui, hover: egui::Color32) {
    take_box_off(ui);
    let widgets = &mut ui.visuals_mut().widgets;
    widgets.hovered.weak_bg_fill = hover;
    widgets.active.weak_bg_fill = hover;
    widgets.open.weak_bg_fill = hover;
}

/// The margin a field's text keeps: `TextEdit`'s own default, spelled out here
/// because the box is Kimo's (`Field`) and it is the frame that carries the
/// margin — a frame egui would otherwise build out of this very number.
const FIELD_MARGIN: egui::Margin = egui::Margin::symmetric(4, 2);

/// The width of a field's box: the one point every hairline in the app is
/// drawn at, and the width egui gives a focused field's own frame.
const FIELD_STROKE: f32 = 1.0;

/// The frame a field wears when Kimo draws the box itself: the text's margin
/// and nothing else — no fill, no line. A frame of its own is also what keeps
/// egui from painting one, and from reaching for `Visuals::selection.stroke` to
/// paint it: that slot is the colour of *selected text* too, so a field that
/// took it for its outline would recolour every selection in the app. (The
/// accent is a fill and focus colour, not text — `theme`'s contrast test says
/// so.)
fn field_frame() -> egui::Frame {
    egui::Frame::NONE.inner_margin(FIELD_MARGIN)
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
/// **`flat` is the field with no box at all**: the answer's own address lines
/// under their mail, and the writing area, whose box is the room the card gives
/// it rather than the field's own height (`compose_body`).
struct Field<'t> {
    edit: egui::TextEdit<'t>,
    enabled: bool,
    flat: bool,
}

impl<'t> Field<'t> {
    fn new(edit: egui::TextEdit<'t>) -> Self {
        Self {
            edit,
            enabled: true,
            flat: false,
        }
    }

    /// The field a worker owns for the moment: greyed out, and no longer taking
    /// the keyboard (`Ui::add_enabled`).
    fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// The field without a box: a line of a letter rather than a form's.
    fn flat(mut self) -> Self {
        self.flat = true;
        self
    }
}

impl egui::Widget for Field<'_> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let Self {
            edit,
            enabled,
            flat,
        } = self;
        let edit = edit.frame(field_frame());
        if flat {
            return ui.add_enabled(enabled, edit);
        }
        // A slot under the text for the box: the box's rect is the field's own,
        // known only once the field has been laid out, and its fill has to be
        // under the text rather than over it.
        let slot = ui.painter().add(egui::Shape::Noop);
        let field = ui.add_enabled(enabled, edit);
        let scheme = theme::of(ui.ctx());
        let visuals = ui.style().interact(&field);
        let stroke = if field.has_focus() {
            egui::Stroke::new(FIELD_STROKE, scheme.accent)
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

/// The reply's own To row on the spot: the field, flat like the From line
/// above it (`Field::flat`), and the chevron at the row's end that folds the
/// Cc and Bcc lines out under it — down while they are away, up once they are
/// under the row. The chevron's cell is taken off the field's own width, so
/// the row ends where every other row's field does.
///
/// Returns the field's response — what puts the keyboard in it on the card's
/// first frame — and the chevron's, which is the caller's to act on.
fn compose_to_row(
    ui: &mut egui::Ui,
    to: &mut String,
    width: f32,
    sending: bool,
    open: bool,
) -> (egui::Response, egui::Response) {
    let scheme = theme::of(ui.ctx());
    ui.horizontal(|ui| {
        ui.add_sized(
            [COMPOSE_LABEL_WIDTH, COMPOSE_FIELD_HEIGHT],
            egui::Label::new("To").halign(egui::Align::RIGHT),
        );
        let room = COMPOSE_FOLD_CELL + ui.spacing().item_spacing.x;
        let edit = egui::TextEdit::singleline(to)
            .hint_text("name@example.com")
            .desired_width(width - room);
        let field = ui.add(Field::new(edit).enabled(!sending).flat());
        let (rect, fold) =
            ui.allocate_exact_size(egui::Vec2::splat(COMPOSE_FOLD_CELL), egui::Sense::click());
        let fold = titlebar_hover(ui, fold);
        paint_fold_chevron(
            ui.painter(),
            rect.center(),
            COMPOSE_FOLD_GLYPH,
            open,
            card_action_color(scheme, fold.hovered()),
        );
        if fold.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let tip = if open {
            "Fold Cc and Bcc away"
        } else {
            "Add Cc and Bcc"
        };
        let fold = fold.on_hover_text(tip);
        (field, fold)
    })
    .inner
}

/// The chevron a flat row of the answer ends with: the To row's, down while
/// the Cc and Bcc lines are folded away — the way to them — and up once they
/// are under the row, and the From list's own (`paint_from_chevron`), which
/// reads the same way over the list it opens. The thread rows' chevron turned
/// a quarter (`paint_chevron`): the same two strokes meeting at a point, and
/// nothing else.
fn paint_fold_chevron(
    painter: &egui::Painter,
    centre: egui::Pos2,
    size: f32,
    open: bool,
    color: egui::Color32,
) {
    let half = size / 2.0;
    let points = if open {
        vec![
            egui::pos2(centre.x - half, centre.y + half * 0.45),
            egui::pos2(centre.x, centre.y - half * 0.55),
            egui::pos2(centre.x + half, centre.y + half * 0.45),
        ]
    } else {
        vec![
            egui::pos2(centre.x - half, centre.y - half * 0.45),
            egui::pos2(centre.x, centre.y + half * 0.55),
            egui::pos2(centre.x + half, centre.y - half * 0.45),
        ]
    };
    painter.add(egui::Shape::line(points, egui::Stroke::new(1.0_f32, color)));
}

/// The chevron the reply's From list ends with on the spot, in the place egui
/// would put the filled triangle a combo box carries: the To row's own drawing
/// (`paint_fold_chevron`), down while the list is away and up while it is open
/// under the row — the same glyph, at the same size, and the same colour under
/// the pointer (`card_action_color`) as the row below it, because the head bar
/// is a letter's line and not a form of boxes.
///
/// `place` is what the combo's own hook handed back — where its triangle would
/// have been, the one spot the button's own layout is known from — and `list`
/// is the button's response, which is what says the pointer is on it. The combo
/// is given a hook that paints nothing: this is what stands in its place.
fn paint_from_chevron(ui: &egui::Ui, list: &egui::Response, place: egui::Rect, open: bool) {
    let scheme = theme::of(ui.ctx());
    paint_fold_chevron(
        ui.painter(),
        place.center(),
        COMPOSE_FOLD_GLYPH,
        open,
        card_action_color(scheme, list.hovered()),
    );
}

/// What an attachment card's click targets came to: the card itself, and — where
/// the card is on the compose form — the small cross in its corner, which is the
/// same thing said out loud rather than left to a click on the card's body.
struct AttachmentCard {
    card: egui::Response,
    close: Option<egui::Response>,
}

/// `action` is the last line of the card's tooltip: what a click on it would do,
/// which is not the same in the two places the card is drawn — in the reading
/// pane it fetches the file, and in the compose card it takes the file off the
/// mail. `removable` draws the cross a file that has not been sent yet can be
/// taken off with, and reserves the room for it: a mail's own attachment is not
/// Kimo's to take away, so the reading pane's cards have no cross at all.
fn attachment_card(
    ui: &mut egui::Ui,
    attachment: &Attachment,
    column: f32,
    action: &str,
    removable: bool,
) -> AttachmentCard {
    let scheme = theme::of(ui.ctx());
    let name_font = egui::FontId::proportional(ATTACHMENT_NAME_FONT_SIZE);
    let meta_font = egui::FontId::proportional(ATTACHMENT_META_FONT_SIZE);
    let meta_text = attachment_meta(attachment);
    // What the card has to hold: the name, and the type and the size under it.
    // It is as wide as the wider of the two wants — a file's name is never cut
    // while the mail's column can hold it — and no wider than that column.
    let (name, meta) = ui.ctx().fonts_mut(|fonts| {
        (
            fonts.layout_no_wrap(attachment.name.clone(), name_font, scheme.link),
            (!meta_text.is_empty())
                .then(|| fonts.layout_no_wrap(meta_text.clone(), meta_font, scheme.text_weak)),
        )
    });
    let text = name
        .size()
        .x
        .max(meta.as_ref().map_or(0.0, |meta| meta.size().x));
    let close_room = if removable {
        ATTACHMENT_CLOSE_SIZE + ATTACHMENT_CARD_PADDING
    } else {
        0.0
    };
    let chrome = ATTACHMENT_GLYPH + ATTACHMENT_CARD_PADDING * 3.0 + close_room;
    let width = (text + chrome).clamp(
        ATTACHMENT_CARD_MIN_WIDTH,
        column.max(ATTACHMENT_CARD_MIN_WIDTH),
    );
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, ATTACHMENT_CARD_HEIGHT),
        egui::Sense::click(),
    );
    if !ui.is_rect_visible(rect) {
        return AttachmentCard {
            card: response,
            close: None,
        };
    }
    let hovered = response.hovered();
    ui.painter().rect_filled(
        rect,
        ATTACHMENT_CARD_CORNER_RADIUS,
        if hovered { scheme.hover } else { scheme.card },
    );
    ui.painter().rect_stroke(
        rect,
        ATTACHMENT_CARD_CORNER_RADIUS,
        egui::Stroke::new(1.0_f32, scheme.border),
        egui::StrokeKind::Inside,
    );

    let glyph = egui::Rect::from_min_size(
        egui::pos2(
            rect.left() + ATTACHMENT_CARD_PADDING,
            rect.center().y - ATTACHMENT_GLYPH / 2.0,
        ),
        egui::vec2(ATTACHMENT_GLYPH, ATTACHMENT_GLYPH),
    );
    paint_file_icon(ui.painter(), glyph, scheme.text_weak);

    let left = glyph.right() + ATTACHMENT_CARD_PADDING;
    // What both lines are drawn in. Only a column too narrow for the name cuts
    // it — the one case the width above could not answer — and the tooltip then
    // carries the whole of it.
    let room = (rect.right() - ATTACHMENT_CARD_PADDING - left - close_room).max(1.0);
    let name = if name.size().x > room {
        truncate_galley(
            ui.ctx(),
            &attachment.name,
            ATTACHMENT_NAME_FONT_SIZE,
            scheme.link,
            room,
        )
    } else {
        name
    };
    let meta = meta.map(|meta| {
        if meta.size().x > room {
            truncate_galley(
                ui.ctx(),
                &meta_text,
                ATTACHMENT_META_FONT_SIZE,
                scheme.text_weak,
                room,
            )
        } else {
            meta
        }
    });

    // The name and what is under it are one block, centred on the card.
    let name_height = name.size().y;
    let name_width = name.size().x;
    let meta_height = meta
        .as_ref()
        .map_or(0.0, |meta| ATTACHMENT_LINE_GAP + meta.size().y);
    let mut y = rect.center().y - (name_height + meta_height) / 2.0;
    ui.painter().galley(egui::pos2(left, y), name, scheme.link);
    if hovered {
        // What a stock link draws while the pointer is on it: a line under the
        // name's own row rather than under the card, which is also what says
        // the click lands on the file name and not on some blank part of it.
        ui.painter().line_segment(
            [
                egui::pos2(left, y + name_height - 1.0),
                egui::pos2(left + name_width, y + name_height - 1.0),
            ],
            egui::Stroke::new(1.0_f32, scheme.link),
        );
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if let Some(meta) = meta {
        y += name_height + ATTACHMENT_LINE_GAP;
        ui.painter()
            .galley(egui::pos2(left, y), meta, scheme.text_weak);
    }

    // The cross, where this card is one a file can be taken off again. It is a
    // widget of its own rather than part of the card's own click — the card's
    // click does the same thing here, and says so in its tooltip — and it takes
    // the treatment the row delete button gets: muted until the pointer is on
    // the button itself, then the scheme's danger fill and the glyph in
    // `on_danger`, because what it does cannot be undone by anything but
    // attaching the file again.
    let close = removable.then(|| {
        let centre = egui::pos2(
            rect.right() - ATTACHMENT_CARD_PADDING - ATTACHMENT_CLOSE_SIZE / 2.0,
            rect.center().y,
        );
        let rect = egui::Rect::from_center_size(centre, egui::Vec2::splat(ATTACHMENT_CLOSE_SIZE));
        let close = ui.interact(rect, response.id.with("close"), egui::Sense::click());
        if close.hovered() {
            ui.painter()
                .rect_filled(rect, ATTACHMENT_CLOSE_RADIUS, scheme.danger_hover);
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        paint_close_icon(
            ui.painter(),
            rect,
            if close.hovered() {
                scheme.on_danger
            } else {
                scheme.text_weak
            },
        );
        close.on_hover_text("Take this file off the mail")
    });

    let hover = if meta_text.is_empty() {
        format!("{}\n{action}", attachment.name)
    } else {
        format!("{}\n{meta_text}\n{action}", attachment.name)
    };
    AttachmentCard {
        card: response.on_hover_text(hover),
        close,
    }
}

/// A card's second line: the MIME type and the size, whichever of the two the
/// mail was stored with. Mail cached before schema v6 has neither — the part it
/// arrived in is not kept, and a sync never refetches a mail the database has —
/// so its card is the file name alone.
fn attachment_meta(attachment: &Attachment) -> String {
    let mut parts = Vec::new();
    if !attachment.content_type.is_empty() {
        parts.push(attachment.content_type.clone());
    }
    if attachment.size > 0 {
        parts.push(human_size(attachment.size));
    }
    parts.join(" · ")
}

/// A size as a card shows it: whole bytes below a kibibyte, then one decimal
/// and the unit — binary ones, the way a file manager on this desktop counts.
/// A trailing `.0` is dropped, so a round size reads as `68 KB`.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    let rounded = format!("{value:.1}");
    format!(
        "{} {}",
        rounded.strip_suffix(".0").unwrap_or(&rounded),
        UNITS[unit]
    )
}

/// The attachment glyph: a page with the top-right corner folded over. One
/// drawing for every kind of file — what a card says about the kind is the MIME
/// type on its second line, which is the sender's own word for it.
fn paint_file_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.3_f32, color);
    let fold = rect.width() * 0.36;
    let (l, t, r, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    p.add(egui::Shape::line(
        vec![
            egui::pos2(l, t),
            egui::pos2(r - fold, t),
            egui::pos2(r, t + fold),
            egui::pos2(r, b),
            egui::pos2(l, b),
            egui::pos2(l, t),
        ],
        stroke,
    ));
    p.add(egui::Shape::line(
        vec![
            egui::pos2(r - fold, t),
            egui::pos2(r - fold, t + fold),
            egui::pos2(r, t + fold),
        ],
        stroke,
    ));
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
/// The labels a mail carries, drawn as one run of chips on the mail's own card:
/// what the user marked it with, said on the mail rather than only in the popup
/// that put it there. Each chip is as wide as its name wants, and the run wraps
/// where the next one does not fit — the way the attachment cards under the body
/// do — so a mail with a handful of labels says so without widening the card.
fn label_chips(ui: &mut egui::Ui, scheme: ColorScheme, labels: &BTreeSet<String>) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(LABEL_CHIP_GAP, LABEL_CHIP_GAP);
        for name in labels {
            let small = egui::TextStyle::Small.resolve(ui.style());
            let galley = ui
                .ctx()
                .fonts_mut(|fonts| fonts.layout_no_wrap(name.clone(), small, scheme.text_weak));
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(
                    galley.size().x + LABEL_CHIP_PADDING * 2.0,
                    LABEL_CHIP_HEIGHT,
                ),
                egui::Sense::hover(),
            );
            ui.painter().rect_filled(
                rect,
                egui::CornerRadius::same(LABEL_CHIP_CORNER_RADIUS),
                scheme.hover,
            );
            ui.painter().galley(
                egui::pos2(
                    rect.center().x - galley.size().x / 2.0,
                    rect.center().y - galley.size().y / 2.0,
                ),
                galley,
                scheme.text_weak,
            );
        }
    });
}

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

/// The heading of the navigation column's label group. A label is not an
/// account: there is nothing to collapse and nothing to count under it as a
/// whole, so the heading is the word alone — at the column's left edge, where
/// the accounts' own names are, with the label rows indented under it the way
/// folders are.
fn label_group_header(ui: &mut egui::Ui, scheme: ColorScheme) {
    const ROW_HEIGHT: f32 = 20.0;
    /// Where the accounts' names start, so the two headings line up.
    const TEXT_LEFT: f32 = 20.0;

    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_HEIGHT),
        egui::Sense::hover(),
    );
    if !ui.is_rect_visible(rect) {
        return;
    }
    let small = egui::TextStyle::Small.resolve(ui.style());
    let name = ui
        .ctx()
        .fonts_mut(|fonts| fonts.layout_no_wrap("Labels".to_string(), small, scheme.text_weak));
    ui.painter().galley(
        egui::pos2(
            rect.left() + TEXT_LEFT,
            rect.center().y - name.size().y / 2.0,
        ),
        name,
        scheme.text_weak,
    );
}

/// One label in the navigation column: the bar's own tag glyph where a folder
/// row has its icon, the name, and how much mail carries it. A selected one
/// takes the selection fill, and a click anywhere on the row is what toggles
/// that — the whole row is the target, for the same reason a folder's is. A
/// label with mail under it reads brighter than one whose mail has all let it
/// go, which is the one thing the count is there to say.
fn label_row(
    ui: &mut egui::Ui,
    scheme: ColorScheme,
    name: &str,
    count: usize,
    selected: bool,
) -> egui::Response {
    const ROW_HEIGHT: f32 = 22.0;
    const GLYPH_BOX: f32 = 16.0;
    const GLYPH: f32 = 14.0;

    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_HEIGHT),
        egui::Sense::click(),
    );
    if !ui.is_rect_visible(rect) {
        return response;
    }
    if selected {
        ui.painter().rect_filled(rect, 3.0_f32, scheme.selection);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 3.0_f32, scheme.hover);
    }
    let color = if selected || count > 0 {
        scheme.text
    } else {
        scheme.text_weak
    };

    let glyph_center = egui::pos2(rect.left() + 4.0 + GLYPH_BOX / 2.0, rect.center().y);
    paint_label_icon(
        ui.painter(),
        egui::Rect::from_center_size(glyph_center, egui::Vec2::splat(GLYPH_BOX)),
        color,
        GLYPH,
    );

    let left = rect.left() + 4.0 + GLYPH_BOX + 6.0;
    let mut right = rect.right() - 6.0;
    if count > 0 {
        let small = egui::TextStyle::Small.resolve(ui.style());
        let text = ui
            .ctx()
            .fonts_mut(|fonts| fonts.layout_no_wrap(count.to_string(), small, color));
        right -= text.size().x;
        ui.painter().galley(
            egui::pos2(right, rect.center().y - text.size().y / 2.0),
            text,
            color,
        );
        right -= 6.0;
    }
    let body = egui::TextStyle::Body.resolve(ui.style());
    let label = truncate_galley(ui.ctx(), name, body.size, color, (right - left).max(1.0));
    ui.painter().galley(
        egui::pos2(left, rect.center().y - label.size().y / 2.0),
        label,
        color,
    );
    if selected {
        response.on_hover_text(format!("Stop filtering by {name}"))
    } else {
        response.on_hover_text(format!("Show the mail labelled {name}"))
    }
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

/// Map a point of a 24×24 outline onto a box of `size` centred on `center`.
fn outline_point(center: egui::Pos2, x: f32, y: f32, size: f32) -> egui::Pos2 {
    center + egui::vec2(x - 12.0, y - 12.0) * (size / 24.0)
}

/// Map a point of one of those 24×24 outlines onto a title-bar cell, centred
/// on `center` and drawn at `MAIL_ACTION_GLYPH`.
fn glyph_point(center: egui::Pos2, x: f32, y: f32) -> egui::Pos2 {
    outline_point(center, x, y, MAIL_ACTION_GLYPH)
}

/// The widget id of one of a row's controls — its checkbox, or the delete button
/// it offers under the pointer — or of the list's select-all row: one per row,
/// stable between frames the way the chevron's key is. `tag` keeps a
/// conversation's controls apart from the ones of the mail it holds — the two can
/// be the same mail — and the account and folder keep a search's hits apart,
/// since two accounts can hold one uid.
fn row_widget_id(tag: &'static str, account: &str, folder: &str, key: &str) -> egui::Id {
    egui::Id::new((tag, account, folder, key))
}

/// The checkbox a row draws in place of its avatar: `Off` is an outline — the
/// accent under the pointer, which is what says it can be clicked — and `On` is
/// a filled box with the scheme's check on it. `Partial`, which only a
/// conversation with some of its mails checked reaches, is the same fill with a
/// dash: the box stands for the whole thread, and the thread is partly in.
fn paint_checkbox(
    painter: &egui::Painter,
    rect: egui::Rect,
    state: CheckState,
    hovered: bool,
    scheme: ColorScheme,
) {
    match state {
        CheckState::Off => {
            let color = if hovered {
                scheme.accent
            } else {
                scheme.text_weak
            };
            painter.rect_stroke(
                rect,
                CHECKBOX_CORNER_RADIUS,
                egui::Stroke::new(1.0_f32, color),
                egui::StrokeKind::Inside,
            );
        }
        CheckState::On | CheckState::Partial => {
            painter.rect_filled(rect, CHECKBOX_CORNER_RADIUS, scheme.accent);
            let stroke = egui::Stroke::new(CHECKBOX_STROKE, scheme.on_accent);
            if state == CheckState::On {
                // The check: a short stroke down into the corner and a long one
                // back up out of it — one path, two turns, and no glyph from
                // the icon set, since a checkbox is not one of the bar's own
                // 24×24 outlines.
                let g = |x: f32, y: f32| {
                    egui::pos2(
                        rect.left() + rect.width() * x,
                        rect.top() + rect.height() * y,
                    )
                };
                painter.add(egui::Shape::line(
                    vec![g(0.26, 0.52), g(0.43, 0.7), g(0.74, 0.32)],
                    stroke,
                ));
            } else {
                let mid = rect.center().y;
                painter.line_segment(
                    [
                        egui::pos2(rect.left() + 4.0, mid),
                        egui::pos2(rect.right() - 4.0, mid),
                    ],
                    stroke,
                );
            }
        }
    }
}

/// The chevron a conversation's row carries at its left: a line drawing — two
/// strokes meeting at a point — rather than a filled triangle, pointing right
/// while the thread's mails are hidden and down while they are under it.
fn paint_chevron(
    painter: &egui::Painter,
    centre: egui::Pos2,
    size: f32,
    open: bool,
    color: egui::Color32,
) {
    let half = size / 2.0;
    let points = if open {
        vec![
            egui::pos2(centre.x - half, centre.y - half * 0.45),
            egui::pos2(centre.x, centre.y + half * 0.55),
            egui::pos2(centre.x + half, centre.y - half * 0.45),
        ]
    } else {
        vec![
            egui::pos2(centre.x - half * 0.45, centre.y - half),
            egui::pos2(centre.x + half * 0.55, centre.y),
            egui::pos2(centre.x - half * 0.45, centre.y + half),
        ]
    };
    painter.add(egui::Shape::line(points, egui::Stroke::new(1.0_f32, color)));
}

/// The left-pointing chevron of the popup's back button: the thread rows'
/// chevron (`paint_chevron`) mirrored, drawn with the stroke the title bar's
/// glyph outlines use. Chevrons are the app's `back / open` glyph — the
/// thread rows' opens a conversation, this one leaves the popup pane — so
/// the one shape is what both of them draw.
fn paint_back_chevron(painter: &egui::Painter, centre: egui::Pos2, color: egui::Color32) {
    let half = POPUP_BACK_CHEVRON / 2.0;
    let points = vec![
        egui::pos2(centre.x + half * 0.45, centre.y - half),
        egui::pos2(centre.x - half * 0.55, centre.y),
        egui::pos2(centre.x + half * 0.45, centre.y + half),
    ];
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(POPUP_BACK_STROKE, color),
    ));
}

/// The paperclip a message row draws when the mail has parts: the wire of a
/// clip, drawn as the one path it is — down, a half turn, up, over the top,
/// down, and a wide half turn back up — the way the title bar's outlines are
/// drawn, at `rect`'s size and in the caller's color.
fn paint_clip_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let scale = rect.width() / 24.0;
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, rect.width());
    let stroke = egui::Stroke::new(2.0_f32 * scale, color);
    let up = egui::vec2(0.0, -1.0);
    let down = egui::vec2(0.0, 1.0);
    let left = egui::vec2(-1.0, 0.0);
    let right = egui::vec2(1.0, 0.0);

    // The inner loop, from its free end at the top: down, round the bottom and
    // up the other side.
    p.add(egui::Shape::line(vec![g(9.6, 8.5), g(9.6, 15.5)], stroke));
    wire_turn(p, g(9.6, 15.5), down, right, 2.4 * scale, stroke);
    p.add(egui::Shape::line(vec![g(14.4, 15.5), g(14.4, 6.0)], stroke));
    // Over the top and down the outside.
    wire_turn(p, g(14.4, 6.0), up, right, 2.05 * scale, stroke);
    p.add(egui::Shape::line(vec![g(18.5, 6.0), g(18.5, 13.5)], stroke));
    // The wide turn under the whole clip, and back up to the last free end.
    wire_turn(p, g(18.5, 13.5), down, left, 6.5 * scale, stroke);
    p.add(egui::Shape::line(vec![g(5.5, 13.5), g(5.5, 7.0)], stroke));
}

/// A half turn of a wire, drawn the way the icon set draws a corner: two quarter
/// turns, each a quadratic whose control is the corner its own tangents meet at
/// (`glyph_tail`), which is what makes one quadratic read as an arc at glyph
/// sizes. The wire arrives at `from` travelling along `dir` — a unit vector —
/// and leaves `2 * radius` away along `normal`, travelling the opposite way,
/// `normal` being the side the turn bulges toward.
fn wire_turn(
    p: &egui::Painter,
    from: egui::Pos2,
    dir: egui::Vec2,
    normal: egui::Vec2,
    radius: f32,
    stroke: egui::Stroke,
) {
    let middle = from + radius * (dir + normal);
    let end = from + 2.0 * radius * normal;
    glyph_tail(p, from, from + radius * dir, middle, stroke);
    glyph_tail(p, middle, end + radius * dir, end, stroke);
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

/// Paper: the contrast mark — a ring with one half of it filled, which is what
/// the cell switches between. Drawn at `size` like the card's other glyphs.
fn paint_paper_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32, size: f32) {
    let centre = outline_point(rect.center(), 12.0, 12.0, size);
    let radius = size * 8.0 / 24.0;
    p.circle_stroke(centre, radius, egui::Stroke::new(1.5_f32, color));

    // The right half, filled: the arc from the ring's top round to its bottom,
    // closed through the centre, so what is painted is a half disc rather than a
    // polygon the ring would have to be cut out of.
    const STEPS: usize = 12;
    let mut half = Vec::with_capacity(STEPS + 2);
    half.push(centre);
    for step in 0..=STEPS {
        let angle =
            -std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * step as f32 / STEPS as f32;
        half.push(centre + egui::vec2(angle.cos(), angle.sin()) * radius);
    }
    p.add(egui::Shape::convex_polygon(half, color, egui::Stroke::NONE));
}

/// Reply: an arrow curving back to the left, its shaft turning down at the
/// tail. Every part is an open path, so the glyph reads as a line drawing like
/// the rest of the bar's icons. `size` is the glyph's own, as in
/// `paint_delete_icon`: the bar's cells draw it at `MAIL_ACTION_GLYPH`, the
/// mail card's smaller ones at `CARD_ACTION_GLYPH`.
fn paint_reply_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32, size: f32) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
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

/// Reply all: reply's arrow with a second head behind the first. Drawn at
/// `size`, like `paint_reply_icon`.
fn paint_reply_all_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32, size: f32) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
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

/// Forward: reply's arrow mirrored — it leaves to the right instead. Drawn at
/// `size`, like `paint_reply_icon`.
fn paint_forward_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32, size: f32) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
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
fn paint_label_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32, size: f32) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
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
    p.circle_filled(g(7.5, 7.5), size * 0.055, color);
}

/// Flag: a pole with a swallow-tailed pennant on it.
/// Flag: a pole with a banner. It is the one glyph of the title bar's set that
/// is drawn outside it as well — a flagged mail's row in the list draws it at
/// the end of its title line — so it takes a `size` like reply's arrow rather
/// than being fixed at `MAIL_ACTION_GLYPH`, and its stroke is scaled with it the
/// way the paperclip's is (the clip is only ever drawn at the row's size).
fn paint_flag_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32, size: f32) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
    let stroke = egui::Stroke::new(1.5_f32 * size / MAIL_ACTION_GLYPH, color);
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

/// Sort: two arrows side by side, one up and one down — the mark every list
/// with an order draws on its sort control. The bar's own, like the funnel: a
/// menu row that names a direction uses one arrow of the pair
/// (`MenuIcon::Newest`), so the cell and the rows are drawn from one idea.
fn paint_sort_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32, size: f32) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
    let stroke = egui::Stroke::new(1.5_f32, color);
    // Up: the shaft, then the head above it.
    p.add(egui::Shape::line(vec![g(9.0, 20.0), g(9.0, 6.0)], stroke));
    p.add(egui::Shape::line(
        vec![g(4.5, 10.5), g(9.0, 6.0), g(13.5, 10.5)],
        stroke,
    ));
    // Down: the same, the other way round.
    p.add(egui::Shape::line(vec![g(15.0, 4.0), g(15.0, 18.0)], stroke));
    p.add(egui::Shape::line(
        vec![g(10.5, 13.5), g(15.0, 18.0), g(19.5, 13.5)],
        stroke,
    ));
}

/// Filter: a funnel, the one glyph of the bar's set that is not about a mail —
/// it says which mail the *list* is showing. Drawn like the rest: one stroked
/// path inside the 24×24 outline the set shares.
fn paint_filter_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32, size: f32) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
    let stroke = egui::Stroke::new(1.5_f32, color);
    p.add(egui::Shape::line(
        vec![
            g(4.0, 6.0),
            g(20.0, 6.0),
            g(14.0, 13.0),
            g(14.0, 20.0),
            g(10.0, 20.0),
            g(10.0, 13.0),
            g(4.0, 6.0),
        ],
        stroke,
    ));
}

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

/// One menu row's glyph, in the 24×24 outline style the whole icon set is drawn
/// in, at `FILTER_ICON_SIZE`.
fn paint_menu_icon(p: &egui::Painter, icon: MenuIcon, rect: egui::Rect, color: egui::Color32) {
    let size = FILTER_ICON_SIZE;
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
    let stroke = egui::Stroke::new(1.5_f32 * size / MAIL_ACTION_GLYPH, color);
    match icon {
        // The whole mailbox: an envelope, the one mark that means *mail* rather
        // than something about it.
        MenuIcon::All | MenuIcon::Unread => {
            p.add(egui::Shape::line(
                vec![
                    g(3.0, 5.0),
                    g(21.0, 5.0),
                    g(21.0, 16.0),
                    g(3.0, 16.0),
                    g(3.0, 5.0),
                ],
                stroke,
            ));
            // The flap, from the top corners down to the middle.
            p.add(egui::Shape::line(
                vec![g(3.0, 6.5), g(12.0, 12.0), g(21.0, 6.5)],
                stroke,
            ));
            if icon == MenuIcon::Unread {
                // A mail that has not been read: the envelope with the new-mail
                // pip at its lower corner, the one part of this set that is
                // filled.
                p.circle_filled(g(19.5, 19.5), 3.0 * size / 24.0, color);
            }
        }
        // The list's own mark for a flagged mail, at the menu's size.
        MenuIcon::Flagged => paint_flag_icon(p, rect, color, size),
        // Addressed to me: a person — a head and the shoulders under it.
        MenuIcon::ToMe | MenuIcon::ByFrom => {
            p.circle_stroke(g(12.0, 8.0), 3.6 * size / 24.0, stroke);
            p.add(egui::Shape::line(
                vec![
                    g(5.0, 19.5),
                    g(5.0, 17.0),
                    g(8.0, 14.0),
                    g(16.0, 14.0),
                    g(19.0, 17.0),
                    g(19.0, 19.5),
                ],
                stroke,
            ));
        }
        // What the mail carries, in the list's own paperclip.
        MenuIcon::HasFiles => paint_clip_icon(p, rect, color),
        // The sort's two directions: one arrow each, the same shaft and head
        // turned round.
        MenuIcon::Newest | MenuIcon::Oldest => {
            let down = icon == MenuIcon::Newest;
            let (tail, head_y) = if down { (5.0, 17.0) } else { (19.0, 7.0) };
            p.add(egui::Shape::line(
                vec![g(12.0, tail), g(12.0, head_y)],
                stroke,
            ));
            let (wing, tip) = if down {
                (head_y - 4.5, head_y)
            } else {
                (head_y + 4.5, head_y)
            };
            p.add(egui::Shape::line(
                vec![g(7.5, wing), g(12.0, tip), g(16.5, wing)],
                stroke,
            ));
        }
        // Sorted by date: a calendar — a box with its binding along the top and
        // the two days' ticks under it.
        MenuIcon::ByDate => {
            p.add(egui::Shape::line(
                vec![
                    g(4.0, 6.5),
                    g(20.0, 6.5),
                    g(20.0, 19.5),
                    g(4.0, 19.5),
                    g(4.0, 6.5),
                ],
                stroke,
            ));
            p.add(egui::Shape::line(vec![g(4.0, 10.5), g(20.0, 10.5)], stroke));
            p.add(egui::Shape::line(vec![g(8.5, 4.5), g(8.5, 8.0)], stroke));
            p.add(egui::Shape::line(vec![g(15.5, 4.5), g(15.5, 8.0)], stroke));
        }
        // Sorted by subject: the title's own two lines, the second shorter.
        MenuIcon::BySubject => {
            p.add(egui::Shape::line(vec![g(4.0, 9.0), g(20.0, 9.0)], stroke));
            p.add(egui::Shape::line(vec![g(4.0, 15.0), g(14.0, 15.0)], stroke));
        }
    }
}

/// One row of either of the bar's menus: the choice's own mark at its left, the
/// name, and — at the row's *end*, where a menu's tick belongs — the check when
/// the choice is the one in force. The whole row is the target, not only the
/// words, and the row is the app's own drawing rather than a stock widget: a
/// checkbox puts its box in front of the name, which is where the mark goes.
///
/// `enabled` is false for a view the session cannot answer — "To me" without an
/// address to match — and such a row neither fills under the pointer nor
/// responds to it.
fn menu_row(
    ui: &mut egui::Ui,
    icon: MenuIcon,
    label: &str,
    ticked: bool,
    enabled: bool,
) -> egui::Response {
    let scheme = theme::of(ui.ctx());
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), FILTER_ROW_HEIGHT), sense);
    let color = if enabled {
        scheme.text
    } else {
        scheme.text_weak
    };
    if response.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(FILTER_ROW_CORNER_RADIUS),
            scheme.hover,
        );
    }
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(
            rect.left() + FILTER_ROW_INSET + FILTER_ICON_SIZE / 2.0,
            rect.center().y,
        ),
        egui::Vec2::splat(FILTER_ICON_SIZE),
    );
    paint_menu_icon(ui.painter(), icon, icon_rect, color);
    // The name, in the interface font the rest of the menu's text is drawn in.
    let font = ui
        .style()
        .text_styles
        .get(&egui::TextStyle::Body)
        .cloned()
        .unwrap_or_else(|| egui::FontId::proportional(14.0));
    ui.painter().text(
        egui::pos2(icon_rect.right() + FILTER_ICON_GAP, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        font,
        color,
    );
    if ticked {
        // The tick is the row's state, and it is the accent's — the colour the
        // bar's own cell takes while any view is on.
        let tick = egui::Rect::from_center_size(
            egui::pos2(
                rect.right() - FILTER_ROW_INSET - FILTER_TICK_SIZE / 2.0,
                rect.center().y,
            ),
            egui::Vec2::splat(FILTER_TICK_SIZE),
        );
        paint_check_icon(ui.painter(), tick, scheme.accent);
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A menu's group heading: the word alone, small and weak — the sort menu's two
/// groups are what it is for, and the filter menu's "All" row is a choice rather
/// than a heading, so it is drawn as one.
fn menu_heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(FILTER_ROW_GAP * 2.0);
    ui.label(egui::RichText::new(text).small().weak());
    ui.add_space(FILTER_ROW_GAP);
}

/// The tick a menu row carries at its end when its view is on: one stroke, the
/// way the rest of the set is drawn.
fn paint_check_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, FILTER_TICK_SIZE);
    let stroke = egui::Stroke::new(1.5_f32 * FILTER_TICK_SIZE / MAIL_ACTION_GLYPH, color);
    p.add(egui::Shape::line(
        vec![g(5.0, 12.5), g(10.0, 17.5), g(19.0, 6.5)],
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
/// close up into a smudge and the can reads better empty. `size` is the glyph's
/// own size: the title bar's cells draw it at `MAIL_ACTION_GLYPH`, a mail row's
/// smaller button at `ROW_ACTION_GLYPH`.
fn paint_delete_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32, size: f32) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
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
    let (y, m, d) = crate::mail::civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
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

    /// The interval the preference asks for, and the clamp it is read through:
    /// nothing in settings.json — a hand-edited file included — gets a sweep
    /// every frame, or one a week.
    #[test]
    fn the_auto_sync_interval_is_the_preference_clamped() {
        let minutes = |n: u64| Duration::from_secs(n * SECS_PER_MINUTE);
        assert_eq!(auto_sync_interval(None), minutes(AUTO_SYNC_MINUTES as u64));
        assert_eq!(auto_sync_interval(Some(0)), minutes(1));
        assert_eq!(auto_sync_interval(Some(30)), minutes(30));
        assert_eq!(auto_sync_interval(Some(100_000)), minutes(1440));
    }

    /// The automatic sync's clock: nothing at all while it is switched off, the
    /// whole interval right after a sweep, and due — never negative — once that
    /// interval has passed.
    #[test]
    fn the_auto_sync_waits_out_its_interval() {
        let start = Instant::now();
        let every = Duration::from_secs(600);
        assert_eq!(auto_sync_wait(false, start, every, start), None);
        assert_eq!(auto_sync_wait(true, start, every, start), Some(every));
        assert_eq!(
            auto_sync_wait(true, start, every, start + Duration::from_secs(599)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            auto_sync_wait(true, start, every, start + every),
            Some(Duration::ZERO)
        );
        assert_eq!(
            auto_sync_wait(true, start, every, start + Duration::from_secs(90_000)),
            Some(Duration::ZERO),
            "a clock that is long overdue is due now, not in the past"
        );
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
            flagged: false,
            message_id: format!("{uid}@example.com"),
            in_reply_to: Vec::new(),
            references: Vec::new(),
            body_text: "body".to_string(),
            body_html: None,
            preview: "preview".to_string(),
            attachments: Vec::new(),
            inline_images: Vec::new(),
            labels: BTreeSet::new(),
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
            smtp: None,
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
        // not about the layout the test is pinning — and so are the labels the
        // machine's database holds, which the app loads at startup.
        app.storage_warning = None;
        app.labels.clear();
        app.label_view = None;
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

    /// The width the built-in renderer holds one mail's body at, for the tests
    /// that read the layout the pane settled on — `None` when it holds none.
    fn held_body_width(app: &KimoApp, identity: &str) -> Option<f32> {
        app.body
            .entries
            .iter()
            .find(|entry| entry.identity == identity)
            .and_then(|entry| entry.width)
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

    /// Every run of text one headless frame drew with the color it was laid out
    /// in, for the tests that care how something reads and not only what it
    /// says. The color a run is drawn in is the one its galley was laid out
    /// with; the color handed to `Painter::galley` is only the fallback.
    fn frame_text_colors(ctx: &egui::Context, app: &mut KimoApp) -> Vec<(String, egui::Color32)> {
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
            collect_text_color(&clipped.shape, &mut texts);
        }
        texts
    }

    /// The boxes of every run of text a frame drew inside `rect` — a galley's own
    /// rect, which is what a row places its lines by, not the ink drawn on it.
    fn text_boxes_in(shapes: &[egui::epaint::ClippedShape], rect: egui::Rect) -> Vec<egui::Rect> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_text_boxes(&clipped.shape, rect, &mut out);
        }
        out
    }

    fn collect_text_boxes(shape: &egui::Shape, rect: egui::Rect, out: &mut Vec<egui::Rect>) {
        match shape {
            egui::Shape::Text(text) => {
                let drawn = egui::Rect::from_min_size(text.pos, text.galley.size());
                if rect.contains_rect(drawn) {
                    out.push(drawn);
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_text_boxes(shape, rect, out);
                }
            }
            _ => {}
        }
    }

    fn collect_text_color(shape: &egui::Shape, out: &mut Vec<(String, egui::Color32)>) {
        match shape {
            egui::Shape::Text(text) => {
                let color = text
                    .galley
                    .job
                    .sections
                    .first()
                    .map_or(text.fallback_color, |section| section.format.color);
                out.push((text.galley.text().to_string(), color));
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_text_color(shape, out);
                }
            }
            _ => {}
        }
    }

    /// Every rectangle one headless frame painted, for the tests that click
    /// something the app drew rather than a position worked out by arithmetic.
    fn frame_rects(ctx: &egui::Context, app: &mut KimoApp) -> Vec<egui::Rect> {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| app.ui(ui));
        output.textures_delta.clear();
        let mut rects = Vec::new();
        for clipped in &output.shapes {
            collect_rects(&clipped.shape, &mut rects);
        }
        rects
    }

    fn collect_rects(shape: &egui::Shape, out: &mut Vec<egui::Rect>) {
        match shape {
            egui::Shape::Rect(rect) => out.push(rect.rect),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_rects(shape, out);
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
            smtp: None,
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
    /// The columns give way to a narrowing window in one order, and each
    /// boundary is where the plan says it is: the navigation collapses while
    /// the list still has its minimum, the pane is capped beside that
    /// minimum, the pane becomes a popup when not even the two minimums fit,
    /// and the list itself scrolls rather than narrowing past its own.
    #[test]
    fn the_columns_give_way_in_one_order() {
        let plan = |window: f32, collapsed: bool| plan_columns(window, 200.0, 460.0, collapsed);

        // A wide window: everything as the splitters left it.
        let wide = plan(1400.0, false);
        assert!(!wide.popup);
        assert!(wide.pane_max.is_none());
        assert!(!wide.list_scrolls);
        assert!(!wide.collapses_nav);

        // One point below navigation + pane + the list's minimum, and the
        // navigation is the column whose width the two others can have.
        assert!(!plan(900.0, false).collapses_nav);
        assert!(plan(899.0, false).collapses_nav);

        // With it gone the list keeps its minimum while the window can spare
        // it: at exactly pane + list the pane still keeps its own width,
        assert!(plan(700.0, true).pane_max.is_none());
        // and below that the pane is capped at the room beside the minimum,
        assert_eq!(plan(699.0, true).pane_max, Some(459.0));
        assert_eq!(plan(600.0, true).pane_max, Some(360.0));
        // down to the two minimums side by side — and not one point further.
        assert!(!plan(520.0, true).popup);
        assert!(plan(519.0, true).popup);

        // The popup boundary only moves with the navigation's own room: a
        // window that cannot hold list and pane beside a navigation cannot
        // hold them as the only two columns either.
        assert!(plan(600.0, false).popup);

        // The list itself never narrows past its minimum; it scrolls.
        assert!(!plan(240.0, true).list_scrolls);
        assert!(plan(239.0, true).list_scrolls);
    }

    /// The same order, live: a window crossing below what the three columns
    /// need collapses the navigation, a narrower one caps the pane beside the
    /// list's minimum, and widening again brings the pane back to the width
    /// its splitter left it at — which egui on its own would have forgotten
    /// in favour of the last width it was capped at.
    #[test]
    fn the_pane_is_capped_and_comes_back_at_its_own_width() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        let settle = |app: &mut KimoApp, width: f32| {
            for _ in 0..30 {
                run_app_frame_sized(&ctx, app, Vec::new(), egui::vec2(width, 900.0));
            }
        };

        // Wide: the defaults, and the list is what the two panels leave.
        settle(&mut app, 1400.0);
        assert!(!app.nav_collapsed);
        assert!(!app.pane_capped);
        assert_eq!(app.pane_width, CONTENT_DEFAULT_WIDTH);
        assert!((app.list_column.width() - 740.0).abs() < 0.5);

        // One point below navigation + pane + the list's minimum: the
        // navigation gives way, and the list keeps well more than its
        // minimum rather than being crushed under the pane.
        settle(&mut app, 899.0);
        assert!(app.nav_collapsed);
        assert!(!app.pane_capped);
        assert!((app.list_column.width() - 439.0).abs() < 0.5);

        // Narrower still: the pane is capped at what leaves the list its
        // minimum, and the width to bring it back to is the app's own.
        settle(&mut app, 600.0);
        assert!(app.pane_capped);
        assert!((app.list_column.width() - LIST_MIN_WIDTH).abs() < 0.5);
        assert_eq!(app.pane_width, CONTENT_DEFAULT_WIDTH);

        // Widening lifts the cap and restores the pane's own width. The
        // navigation stays collapsed — re-expanding is a manual choice, the
        // way it always was — so the list has everything but the pane.
        settle(&mut app, 1400.0);
        assert!(!app.pane_capped);
        assert!((app.list_column.width() - 940.0).abs() < 0.5);
        assert_eq!(app.pane_width, CONTENT_DEFAULT_WIDTH);
    }

    /// Below the two minimums side by side, the reading pane stops being a
    /// column: the list has the window, a click on a mail opens the pane over
    /// it, and the band under the title bar is the way back to the list.
    #[test]
    fn the_reading_pane_becomes_a_popup_over_the_list() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        for _ in 0..10 {
            run_app_frame_sized(&ctx, &mut app, Vec::new(), egui::vec2(500.0, 900.0));
        }
        assert!(app.pane_popup);
        assert!(app.nav_collapsed, "the navigation gave way long before");
        // The list has the whole window — the pane over it is an overlay, not
        // a column taking part of it.
        assert!((app.list_column.width() - 500.0).abs() < 0.5);

        // A click on a mail opens it, over the list and under the title bar,
        // and the list's own rect is unchanged: nothing was taken from it.
        click_at_sized(&ctx, &mut app, egui::pos2(250.0, 74.0), 500.0);
        assert_eq!(app.selected_message.as_deref(), Some("0"));
        assert!((app.list_column.width() - 500.0).abs() < 0.5);

        // The way back is a round button floating over the mail's top-left
        // corner — a circle where the plan puts it, and a blurred shape
        // behind it: the shadow is what makes it read as floating rather
        // than as a hole in the mail.
        let shapes = frame_shapes_at(&ctx, &mut app, 500.0);
        let centre = egui::pos2(
            POPUP_BACK_MARGIN + POPUP_BACK_DIAMETER / 2.0,
            TITLEBAR_HEIGHT + POPUP_BACK_MARGIN + POPUP_BACK_DIAMETER / 2.0,
        );
        assert!(
            shapes.iter().any(|clipped| match &clipped.shape {
                egui::Shape::Circle(circle) =>
                    circle.center.distance(centre) < 1.0
                        && (circle.radius - POPUP_BACK_DIAMETER / 2.0).abs() < 1.0,
                _ => false,
            }),
            "the back button is a circle where the popup puts it"
        );
        let button = egui::Rect::from_center_size(centre, egui::Vec2::splat(POPUP_BACK_DIAMETER));
        assert!(
            shapes.iter().any(|clipped| match &clipped.shape {
                egui::Shape::Rect(shadow) =>
                    shadow.blur_width > 0.0 && shadow.rect.intersects(button),
                _ => false,
            }),
            "a blurred shape behind the button is its shadow"
        );

        // A click on it closes the mail, and the list is what is on screen
        // again.
        click_at_sized(&ctx, &mut app, centre, 500.0);
        assert!(
            app.selected_message.is_none(),
            "the way back closed the mail"
        );

        // Narrower than the list's own minimum, the list keeps that minimum
        // and scrolls sideways instead of crushing its rows.
        for _ in 0..5 {
            run_app_frame_sized(&ctx, &mut app, Vec::new(), egui::vec2(200.0, 900.0));
        }
        assert!(app.list_scrolls);
        assert!(app.pane_popup);
    }

    /// One headless frame of an app under test, with the given input events.
    fn run_app_frame(ctx: &egui::Context, app: &mut KimoApp, events: Vec<egui::Event>) {
        run_app_frame_sized(ctx, app, events, egui::vec2(1400.0, 900.0));
    }

    /// The same frame, in a window of the size given — what the layout tests
    /// below resize the window with.
    fn run_app_frame_sized(
        ctx: &egui::Context,
        app: &mut KimoApp,
        events: Vec<egui::Event>,
        size: egui::Vec2,
    ) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| app.ui(ui));
        output.textures_delta.clear();
    }

    /// An export answers with the path the worker wrote, and the desktop is told
    /// where it went: the two lines a notification shows, and nothing above the
    /// message list. The path is the whole of it — the name is the dialog's or the
    /// writer's, and neither is known until the file is on disk. The line itself
    /// is not raised here (the arm calls `crate::notify::show`, which posts to
    /// the thread that talks to a real daemon), so what is pinned is what it says.
    #[test]
    fn a_finished_export_says_where_the_file_went() {
        let (summary, body) = exported_notification(Path::new("/home/ada/Desktop/Invoice 42.eml"));
        assert_eq!(summary, "Mail exported");
        assert_eq!(body, "Saved to /home/ada/Desktop/Invoice 42.eml");
    }

    /// The other way round, for the file the app writes without being asked
    /// where: a downloaded attachment's line above the message list — the line
    /// the export used to get — and the timing that takes it away again. The
    /// line is one of two things the app says out loud on its own (`sent_notice`
    /// is the other), and a line that stayed would be furniture.
    #[test]
    fn the_line_above_the_list_says_where_a_download_went_and_is_timed() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.apply_event(Event::AttachmentDownloaded {
            path: std::path::PathBuf::from("/home/ada/Downloads/report.pdf"),
        });

        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts
                .iter()
                .any(|text| text == "Saved to /home/ada/Downloads/report.pdf"),
            "{texts:?}"
        );

        // Past its time the line is gone, and the app has stopped asking to be
        // repainted for it.
        let stale = Instant::now()
            .checked_sub(NOTICE_TIME + Duration::from_millis(1))
            .expect("the machine has been up longer than the notice lasts");
        app.notice = app.notice.take().map(|(text, _)| (text, stale));
        let texts = frame_texts(&ctx, &mut app);
        assert!(app.notice.is_none(), "the notice outlived its time");
        assert!(
            !texts.iter().any(|text| text.starts_with("Saved to")),
            "{texts:?}"
        );
    }

    /// The attachment cards a frame drew, in the order they were laid out: the
    /// *filled* rects whose height is a card's — a card is a fill and a stroke,
    /// and the stroke is a rect of its own — in the reading pane and not in the
    /// column beside it, where a height of their own says they are cards.
    fn attachment_cards_in(ctx: &egui::Context, app: &mut KimoApp) -> Vec<egui::Rect> {
        let pane_left = app.list_column.right();
        fills_in_color(&frame_shapes_at(ctx, app, 1400.0), theme::of(ctx).card)
            .into_iter()
            .filter(|rect| {
                (rect.height() - ATTACHMENT_CARD_HEIGHT).abs() < 0.5 && rect.center().x > pane_left
            })
            .collect()
    }
    /// Every attachment is a card under the body: the file's name, and the type
    /// and size the mail was fetched with under it. A mail cached before the
    /// database kept those two is the name alone — no second line, and no
    /// separator left standing on its own.
    #[test]
    fn every_attachment_is_a_card_under_the_body() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.states
            .get_mut("acct")
            .expect("the fixture account")
            .mailbox
            .folders[0]
            .messages[0]
            .attachments = vec![
            Attachment {
                name: "report.pdf".to_string(),
                content_type: "application/pdf".to_string(),
                size: 68 * 1024,
            },
            Attachment {
                name: "cached-before-v6.png".to_string(),
                content_type: String::new(),
                size: 0,
            },
        ];
        app.selected_account = Some("acct".to_string());
        app.selected_folder = Some("INBOX".to_string());
        app.selected_message = Some("0".to_string());

        // The first frame is what sizes the pane; the mail is drawn from the
        // second one on, the way the app draws it live.
        let _ = frame_texts(&ctx, &mut app);
        let texts = frame_texts(&ctx, &mut app);

        let position = |text: &str| {
            texts
                .iter()
                .position(|drawn| drawn == text)
                .unwrap_or_else(|| panic!("{text:?} is not on the card: {texts:?}"))
        };
        let body = position("body");
        assert!(
            body < position("report.pdf"),
            "the cards come after the body: {texts:?}"
        );
        assert!(
            position("application/pdf · 68 KB") < position("cached-before-v6.png"),
            "the cards keep the order the parts had: {texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text.starts_with('·')),
            "the old row left a separator behind: {texts:?}"
        );

        // The names are links: drawn in the scheme's link color rather than in
        // the body text's, which is what says a click is offered.
        let link = theme::of(&ctx).link;
        let colors = frame_text_colors(&ctx, &mut app);
        for name in ["report.pdf", "cached-before-v6.png"] {
            assert!(
                colors
                    .iter()
                    .any(|(drawn, color)| drawn == name && *color == link),
                "{name} is not drawn as a link: {colors:?}"
            );
        }
    }

    /// Whether a sync starts within `window`: the worker sends `SyncStarted`
    /// before it touches the network, so a sweep the app asked for shows up on
    /// the channel at once. Whatever is already on the channel is dropped first,
    /// so a sweep of an earlier frame cannot answer for this one — the caller
    /// drains before the frame it is asking about, never after.
    fn sync_starts_within(app: &mut KimoApp, window: Duration) -> bool {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            match app.rx.try_recv() {
                Ok(Event::SyncStarted { .. }) => return true,
                Ok(_) => {}
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return false,
            }
        }
        false
    }

    /// The automatic sync is what its preference says: a sweep of every account
    /// once the interval since the last one is up, and nothing before that. The
    /// preference is the whole switch — the title bar's Sync, and the startup
    /// sweep, are not this clock.
    #[test]
    fn the_automatic_sync_sweeps_when_its_interval_is_up() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        // A port with nothing behind it: the sweep reaches the job, which fails
        // to connect and says so on the account. What the clock does is the
        // question here, not what the server answers.
        app.accounts[0].host = "127.0.0.1".to_string();
        app.accounts[0].port = 1;
        app.auto_sync_every = Duration::from_secs(10 * SECS_PER_MINUTE);

        // One frame with the clock where the case wants it: what is on the
        // channel beforehand is dropped, and what arrives after is the answer.
        let frame = |app: &mut KimoApp| {
            while app.rx.try_recv().is_ok() {}
            let _ = frame_texts(&ctx, app);
        };

        // Switched off: an overdue clock sweeps nothing.
        app.auto_synced_at = Instant::now() - Duration::from_secs(3600);
        frame(&mut app);
        assert!(
            !sync_starts_within(&mut app, Duration::from_millis(300)),
            "the preference is off"
        );

        // Switched on, and not yet due: nothing either.
        app.auto_sync = true;
        app.auto_synced_at = Instant::now();
        frame(&mut app);
        assert!(
            !sync_starts_within(&mut app, Duration::from_millis(300)),
            "the interval has not passed"
        );

        // Due: every account is asked.
        app.auto_synced_at = Instant::now() - Duration::from_secs(601);
        frame(&mut app);
        assert!(
            sync_starts_within(&mut app, Duration::from_secs(5)),
            "the sweep starts once its interval is up"
        );

        // And the preference has the last word: switched off, an overdue clock
        // sweeps nothing, however long it has been overdue.
        app.auto_sync = false;
        app.auto_synced_at = Instant::now() - Duration::from_secs(3600);
        frame(&mut app);
        assert!(
            !sync_starts_within(&mut app, Duration::from_millis(300)),
            "nothing sweeps on a clock nobody asked for"
        );
    }

    /// The title bar's Sync asks for a sweep at any time — the app syncs at
    /// startup on its own, and this is the same sweep by hand. It is what the
    /// cell means now: "sync now", not "the mode that needs a manual sync".
    #[test]
    fn the_sync_cell_asks_for_a_sweep_at_any_time() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        // A port with nothing behind it, so the job fails at connect rather
        // than reaching a server: that it *starts* is the question.
        app.accounts[0].host = "127.0.0.1".to_string();
        app.accounts[0].port = 1;
        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());
        while app.rx.try_recv().is_ok() {}

        let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, TITLEBAR_HEIGHT));
        let rects = titlebar_rects(bar, 3);
        // The cell is the bar's own kind of button: it fills under the pointer
        // with the bar's rounded fill, like the cog beside it.
        let filled = fill_under_the_pointer(
            &ctx,
            &mut app,
            1400.0,
            rects.sync.center(),
            TITLEBAR_BUTTON_SIZE,
        )
        .unwrap_or_else(|| panic!("no sync cell at {:?}", rects.sync.center()));
        assert_eq!(
            filled.corner_radius,
            egui::CornerRadius::same(TITLEBAR_BUTTON_CORNER_RADIUS)
        );

        click_at(&ctx, &mut app, rects.sync.center());
        assert!(
            sync_starts_within(&mut app, Duration::from_secs(5)),
            "the sync cell starts a sweep"
        );
    }
    /// The attachment cards are one run across the mail's column, not one card
    /// per line: a fixed width each, two to a row where they fit, the third
    /// wrapped onto the next row at the column's own left edge.
    #[test]
    fn the_attachment_cards_wrap_across_the_mails_column() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        let part = |name: &str| Attachment {
            name: name.to_string(),
            content_type: "application/pdf".to_string(),
            size: 68 * 1024,
        };
        let three = vec![part("one.pdf"), part("two.png"), part("three.pdf")];
        app.states
            .get_mut("acct")
            .expect("the fixture account")
            .mailbox
            .folders[0]
            .messages[0]
            .attachments = three.clone();
        app.selected_account = Some("acct".to_string());
        app.selected_folder = Some("INBOX".to_string());
        app.selected_message = Some("0".to_string());

        // The pane sizes the first frame; the cards are on screen from the
        // second one on, the way the app draws them live.
        let _ = frame_texts(&ctx, &mut app);
        let cards = attachment_cards_in(&ctx, &mut app);
        assert_eq!(cards.len(), three.len(), "one card per part: {cards:?}");
        for card in &cards {
            assert!(
                card.width() >= ATTACHMENT_CARD_MIN_WIDTH - 0.5,
                "a card is at least the floor wide, whatever it holds: {cards:?}"
            );
        }
        // The cards sharing a top edge are one row, in the order they were laid
        // out.
        let mut rows: Vec<Vec<egui::Rect>> = Vec::new();
        for card in cards {
            match rows.last_mut() {
                Some(row) if (row[0].top() - card.top()).abs() < 0.5 => row.push(card),
                _ => rows.push(vec![card]),
            }
        }
        assert_eq!(rows.len(), 2, "three cards, two rows: {rows:?}");
        assert_eq!(rows[0].len(), 2, "two of them fit side by side: {rows:?}");
        assert_eq!(rows[1].len(), 1, "the third one wraps: {rows:?}");
        let between = rows[0][1].left() - rows[0][0].right();
        assert!(
            (between - ATTACHMENT_CARD_GAP).abs() < 0.5,
            "two cards are {between} pt apart: {rows:?}"
        );
        assert!(
            (rows[1][0].left() - rows[0][0].left()).abs() < 0.5,
            "the wrapped row starts at the column's own left edge: {rows:?}"
        );
        let drop = rows[1][0].top() - rows[0][0].bottom();
        assert!(
            (drop - ATTACHMENT_CARD_GAP).abs() < 0.5,
            "the wrapped row is {drop} pt under the one above: {rows:?}"
        );

        // Two parts fit on one row: nothing wraps while there is room.
        app.states
            .get_mut("acct")
            .expect("the fixture account")
            .mailbox
            .folders[0]
            .messages[0]
            .attachments = three[..2].to_vec();
        let _ = frame_texts(&ctx, &mut app);
        let cards = attachment_cards_in(&ctx, &mut app);
        assert_eq!(cards.len(), 2, "{cards:?}");
        assert!(
            (cards[0].top() - cards[1].top()).abs() < 0.5,
            "the two share a row: {cards:?}"
        );
    }

    /// A card is as wide as the file's name wants rather than a fixed width, and
    /// the name is drawn whole: the ellipsis is only for a name the mail's own
    /// column cannot hold.
    #[test]
    fn an_attachment_card_shows_the_whole_name() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        let part = |name: &str| Attachment {
            name: name.to_string(),
            content_type: "application/pdf".to_string(),
            size: 68 * 1024,
        };
        let long = "quarterly-report-2026-with-notes.pdf";
        app.states
            .get_mut("acct")
            .expect("the fixture account")
            .mailbox
            .folders[0]
            .messages[0]
            .attachments = vec![part("chart.png"), part(long)];
        app.selected_account = Some("acct".to_string());
        app.selected_folder = Some("INBOX".to_string());
        app.selected_message = Some("0".to_string());

        // The pane sizes the first frame; the cards are on screen from the
        // second one on, the way the app draws them live.
        let _ = frame_texts(&ctx, &mut app);
        let texts = frame_texts(&ctx, &mut app);
        // Both names are drawn verbatim: a cut one would end in the ellipsis
        // instead, and no text of either name would be there to find.
        for name in ["chart.png", long] {
            assert!(
                texts.iter().any(|text| text == name),
                "{name} is on its card whole: {texts:?}"
            );
        }

        let cards = attachment_cards_in(&ctx, &mut app);
        assert_eq!(cards.len(), 2, "one card per part: {cards:?}");
        let widest = cards.iter().map(|card| card.width()).fold(0.0, f32::max);
        let narrowest = cards
            .iter()
            .map(|card| card.width())
            .fold(f32::MAX, f32::min);
        assert!(
            widest > narrowest + 20.0,
            "the name is what a card is as wide as: {cards:?}"
        );

        // No cross on any of them: a mail's own attachment is not Kimo's to take
        // away, so the reading pane's cards are the ones the compose form's are
        // shaped like and nothing more (`attachment_card`).
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let area = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
        let strokes = stroke_centres(&shapes, area);
        for card in &cards {
            assert!(
                !strokes.iter().any(|centre| card.contains(*centre)),
                "the pane's cards carry no button: {card:?} {strokes:?}"
            );
        }
    }
    /// Clicking an attachment card starts a download: the app finds the account
    /// and the mail the pane is showing, and hands the worker the part's place
    /// and the file's name. What the worker does with them is `sync`'s own
    /// test — this one is the click reaching it, which the account's spinner
    /// (`Event::SyncStarted`, the first thing the job sends) is the sign of.
    #[test]
    fn clicking_an_attachment_card_starts_a_download() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.states
            .get_mut("acct")
            .expect("the fixture account")
            .mailbox
            .folders[0]
            .messages[0]
            .attachments = vec![Attachment {
            name: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size: 68 * 1024,
        }];
        // Where the account is does not matter to the click, and the worker's
        // own failure is its own: a port with nothing on it keeps the job from
        // reaching off this machine.
        app.accounts[0].host = "127.0.0.1".to_string();
        app.accounts[0].port = 1;
        app.selected_account = Some("acct".to_string());
        app.selected_folder = Some("INBOX".to_string());
        app.selected_message = Some("0".to_string());

        let _ = frame_texts(&ctx, &mut app);
        let card = frame_rects(&ctx, &mut app)
            .into_iter()
            .find(|rect| (rect.height() - ATTACHMENT_CARD_HEIGHT).abs() < 0.5)
            .expect("the attachment card is on screen");
        let centre = card.center();
        run_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(centre)]);
        for pressed in [true, false] {
            run_app_frame(
                &ctx,
                &mut app,
                vec![egui::Event::PointerButton {
                    pos: centre,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                }],
            );
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut started = false;
        while Instant::now() < deadline && !started {
            match app.rx.try_recv() {
                Ok(Event::SyncStarted { account_id }) => {
                    assert_eq!(account_id, "acct");
                    started = true;
                }
                Ok(_) => {}
                Err(mpsc::TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(5)),
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        assert!(started, "a click on the card did not start a download");
    }

    /// A mail that came with parts says so in the list: a paperclip at the end
    /// of its title line, drawn the same way for a folder row and for a search
    /// hit. A mail with none draws none — and the search row says it with the
    /// clip rather than with a second, textual paperclip in its third line.
    #[test]
    fn a_mail_with_attachments_carries_a_clip_in_the_list() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);

        let with_parts = |app: &mut KimoApp, count: usize| {
            let state = app.states.get_mut("acct").expect("the fixture account");
            state.mailbox.folders[0].messages[0].attachments = (0..count)
                .map(|index| Attachment {
                    name: format!("file-{index}.pdf"),
                    content_type: "application/pdf".to_string(),
                    size: 1024,
                })
                .collect();
            // The list derives its rows from a revision, so a test that changes
            // a mail behind the app's back has to say so — the way a fetched
            // batch does (`AccountUiState::folders_changed`).
            state.folders_changed();
        };

        // A frame first: the list records its own rect as it draws.
        let _ = frame_texts(&ctx, &mut app);
        let mut column = app.list_column;
        let strokes = list_glyph_strokes(&frame_shapes_at(&ctx, &mut app, 1400.0), column);
        assert!(strokes.is_empty(), "an empty mail drew a clip: {strokes:?}");

        // One file: one clip, at the end of the title line — right of the
        // subject and inside the row's own text column.
        with_parts(&mut app, 1);
        column = app.list_column;
        let strokes = list_glyph_strokes(&frame_shapes_at(&ctx, &mut app, 1400.0), column);
        let clip = strokes
            .iter()
            .fold(None::<egui::Rect>, |bounds, rect| {
                Some(bounds.map_or(*rect, |b| b.union(*rect)))
            })
            .expect("the row of a mail with a file draws a clip");
        assert!(
            clip.width() > ROW_CLIP_WIDTH - 2.0
                && clip.width() <= ROW_CLIP_SIZE
                && clip.height() > ROW_CLIP_SIZE - 6.0
                && clip.height() <= ROW_CLIP_SIZE,
            "the clip fills the box the row reserves for it: {clip:?}"
        );
        assert!(
            clip.right() > column.right() - 30.0 && clip.right() <= column.right(),
            "the clip sits at the end of the title line: {clip:?} in {column:?}"
        );
        assert!(
            clip.center().y > column.top() + TITLEBAR_HEIGHT
                && clip.center().y < column.top() + TITLEBAR_HEIGHT + MESSAGE_ROW_HEIGHT,
            "and on the row itself rather than above or below it: {clip:?}"
        );

        // The search row draws the same clip and no longer spells it out.
        app.search.text = "report".to_string();
        app.search_hits = vec![SearchHit {
            account_id: "acct".to_string(),
            folder: "INBOX".to_string(),
            uid: "0".to_string(),
            subject: "Subject 0".to_string(),
            from: "Ada <ada@example.com>".to_string(),
            to: "me@example.com".to_string(),
            date: "2026-01-01".to_string(),
            timestamp: Some(1_700_000_000),
            seen: false,
            flagged: false,
            attachments: 1,
            snippet: "a line with report in it".to_string(),
        }];
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            !texts.iter().any(|text| text.contains('📎')),
            "the search row still spells the paperclip out: {texts:?}"
        );
        let column = app.list_column;
        let strokes = list_glyph_strokes(&frame_shapes_at(&ctx, &mut app, 1400.0), column);
        assert!(
            !strokes.is_empty(),
            "a search hit with a file draws no clip: {strokes:?}"
        );
    }

    /// The row's three lines are one block, centred in the row: the room above
    /// the sender's line is the room below the preview. Placing them from the
    /// row's top edge instead left the preview half as far from the bottom as
    /// the sender was from the top, which is what the eye reads as a row whose
    /// mail is falling out of it.
    #[test]
    fn the_rows_three_lines_are_centred_in_the_row() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 2);
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let column = app.list_column;
        let row = egui::Rect::from_min_size(
            egui::pos2(column.left(), column.top() + TITLEBAR_HEIGHT),
            egui::vec2(column.width(), MESSAGE_ROW_HEIGHT),
        );

        // Everything the row draws text with, the avatar's initial included:
        // the block the eye sees is all of it.
        let lines = text_boxes_in(&shapes, row);
        assert!(
            lines.len() >= 4,
            "the row's sender, date, subject and preview: {lines:?}"
        );
        let top = lines
            .iter()
            .fold(f32::INFINITY, |top, line| top.min(line.top()));
        let bottom = lines
            .iter()
            .fold(f32::NEG_INFINITY, |bottom, line| bottom.max(line.bottom()));
        let middle = (top + bottom) / 2.0;
        assert!(
            (middle - row.center().y).abs() < 2.5,
            "the row's text is centred on {}, not {middle}: {lines:?}",
            row.center().y
        );
    }

    /// The list's card is as tall as what the folder holds, not as tall as the
    /// window: three mails leave the chrome showing below them, a folder that
    /// outgrows the window fills the column as it always did, and a folder with
    /// nothing in it has no card at all.
    #[test]
    fn the_lists_card_is_as_tall_as_the_mail_it_holds() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let content = theme::of(&ctx).content;

        let mut app = app_with_messages(&ctx, 3);
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let column = app.list_column;
        let card = list_card_in(&shapes, column, content).expect("the list sits on a card");
        // The bar's height the first mail starts below, the rows, and nothing
        // below the last of them: the card ends with the mail.
        let expected = TITLEBAR_HEIGHT + 3.0 * MESSAGE_ROW_HEIGHT;
        assert!(
            (card.height() - expected).abs() < 1.0,
            "the card is {} tall, expected {expected}: {card:?}",
            card.height()
        );
        assert!(
            card.bottom() < column.bottom() - 1.0,
            "the chrome shows below the last mail: {card:?}"
        );

        // A folder that outgrows the window: the card fills the column.
        let mut app = app_with_messages(&ctx, 40);
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let column = app.list_column;
        let card = list_card_in(&shapes, column, content).expect("the list sits on a card");
        assert!(
            (card.bottom() - column.bottom()).abs() < 1.0,
            "a list taller than the column fills it: {card:?} in {column:?}"
        );

        // Nothing to list: no card, just the chrome the placeholder stands on.
        let mut app = app_with_messages(&ctx, 0);
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let column = app.list_column;
        assert!(
            list_card_in(&shapes, column, content).is_none(),
            "an empty folder leaves the chrome alone"
        );
    }
    /// Every row is inset by the chevron's column, a mail with nothing to open
    /// included: the avatars line up down the whole list, and the mail that has
    /// nothing to expand simply leaves the column empty.
    #[test]
    fn every_row_is_inset_by_the_chevron_column() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_threaded_mail(
            &ctx,
            vec![
                threaded_mail("3", "Re: Hello", "c@example.com", &["a@example.com"], 300),
                threaded_mail("1", "Hello", "a@example.com", &[], 100),
                threaded_mail("4", "Groceries", "d@example.com", &[], 50),
            ],
        );

        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let column = app.list_column;
        let avatars = avatars_in(&shapes, column);
        assert_eq!(
            avatars.len(),
            2,
            "a conversation's row and a mail's, one avatar each: {avatars:?}"
        );
        assert_eq!(
            avatars[0].0.x, avatars[1].0.x,
            "the two rows start at the same x: {avatars:?}"
        );
        for (_, diameter) in &avatars {
            assert_eq!(
                *diameter, AVATAR_DIAMETER,
                "a row that is not one mail of an open conversation draws the full avatar"
            );
        }
        let expected =
            column.left() + LIST_PADDING.left as f32 + THREAD_INDENT + 8.0 + AVATAR_DIAMETER / 2.0;
        assert!(
            (avatars[0].0.x - expected).abs() < 0.5,
            "the avatar is at {}, expected {expected}",
            avatars[0].0.x
        );
    }

    /// A mail of an open conversation is drawn as one of the conversation's
    /// own: a step further in than the rows around it, with a smaller avatar,
    /// and no guide line down the chevron's column — the step and the smaller
    /// circle are what say, at a glance, that it belongs to the conversation
    /// above it.
    #[test]
    fn the_mails_of_an_open_conversation_step_in_with_a_smaller_avatar() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_one_conversation(&ctx);
        // A frame to derive the rows, then the conversation is opened the way
        // the chevron opens it.
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
        let key = app
            .list_rows
            .as_ref()
            .expect("the list derived its rows")
            .threads[0]
            .key
            .clone();
        app.open_threads.insert(key);
        app.open_rev += 1;

        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let column = app.list_column;
        let avatars = avatars_in(&shapes, column);
        assert_eq!(
            avatars.len(),
            3,
            "the conversation's row and the two mails under it: {avatars:?}"
        );
        let (row, mails) = (avatars[0], &avatars[1..]);
        assert_eq!(
            row.1, AVATAR_DIAMETER,
            "the conversation's own row keeps the full avatar"
        );
        for (centre, diameter) in mails {
            assert_eq!(
                *diameter, THREAD_AVATAR_DIAMETER,
                "a mail of the conversation draws the smaller avatar: {avatars:?}"
            );
            assert!(
                centre.x > row.0.x + 8.0,
                "the mail steps in past the conversation's row: {avatars:?}"
            );
        }

        // Nothing is drawn down the chevron's column: what marks the mails as
        // the conversation's is the step and the smaller avatar, not a line.
        let lines = vertical_lines_in(&shapes, column);
        assert!(
            lines.is_empty(),
            "a mail of an open conversation still draws a line down the column: {lines:?}"
        );
    }

    /// Every vertical line a frame drew inside the list column, away from the
    /// column's own sides — where the rows' hairlines run from the top of the
    /// list to the bottom of it. The guide line the mails of a thread used to
    /// draw down the chevron's column was one of these.
    fn vertical_lines_in(
        shapes: &[egui::epaint::ClippedShape],
        column: egui::Rect,
    ) -> Vec<egui::Rect> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_vertical_lines(&clipped.shape, column, &mut out);
        }
        out
    }

    fn collect_vertical_lines(shape: &egui::Shape, column: egui::Rect, out: &mut Vec<egui::Rect>) {
        match shape {
            egui::Shape::LineSegment { points, .. } => {
                let bounds = egui::Rect::from_two_pos(points[0], points[1]);
                if bounds.width() < 0.6
                    && bounds.height() > MESSAGE_ROW_HEIGHT / 2.0
                    && bounds.center().x > column.left() + 2.0
                    && bounds.center().x < column.right() - 2.0
                {
                    out.push(bounds);
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_vertical_lines(shape, column, out);
                }
            }
            _ => {}
        }
    }

    /// The avatars a frame drew in the message list — each with its diameter,
    /// since a mail of an open conversation draws a smaller one — the filled
    /// circles the rows put their sender's initial on.
    fn avatars_in(
        shapes: &[egui::epaint::ClippedShape],
        column: egui::Rect,
    ) -> Vec<(egui::Pos2, f32)> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_avatars(&clipped.shape, column, &mut out);
        }
        out
    }

    fn collect_avatars(shape: &egui::Shape, column: egui::Rect, out: &mut Vec<(egui::Pos2, f32)>) {
        match shape {
            egui::Shape::Circle(circle) => {
                let diameter = circle.radius * 2.0;
                let drawn = [AVATAR_DIAMETER, THREAD_AVATAR_DIAMETER]
                    .into_iter()
                    .any(|size| (diameter - size).abs() < 0.5);
                if drawn && circle.center.y > TITLEBAR_HEIGHT && column.contains(circle.center) {
                    out.push((circle.center, diameter));
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_avatars(shape, column, out);
                }
            }
            _ => {}
        }
    }

    /// One mail of a folder the threading tests hand to the list: the headers a
    /// conversation is stitched from are arguments, and the folder's own order
    /// is newest first, which is what the timestamp says.
    fn threaded_mail(
        uid: &str,
        subject: &str,
        id: &str,
        references: &[&str],
        ts: i64,
    ) -> MailMessage {
        let mut mail = test_message(uid, false);
        mail.subject = subject.to_string();
        mail.message_id = id.to_string();
        mail.in_reply_to = references
            .iter()
            .rev()
            .take(1)
            .map(|id| id.to_string())
            .collect();
        mail.references = references.iter().map(|id| id.to_string()).collect();
        mail.timestamp = Some(ts);
        mail.date = String::new();
        mail.body_text = format!("body of {uid}");
        // The preview and the body differ on purpose: the list shows the first
        // and the reading pane the second, and a test that reads the frame
        // cannot tell them apart otherwise.
        mail.preview = format!("preview of {uid}");
        mail
    }

    /// A folder holding exactly these mails, with the pane on it.
    fn app_with_threaded_mail(ctx: &egui::Context, mut mails: Vec<MailMessage>) -> KimoApp {
        mails.sort_by_key(|mail| std::cmp::Reverse(mail.timestamp));
        let mut app = app_with_messages(ctx, 0);
        let state = app.states.get_mut("acct").expect("the fixture account");
        state.mailbox.folders[0].messages = mails;
        state.folders_changed();
        app.selected_account = Some("acct".to_string());
        app.selected_folder = Some("INBOX".to_string());
        app
    }

    /// Click at a position the way a user does: the pointer moves there, then
    /// the button goes down and up.
    fn click_at(ctx: &egui::Context, app: &mut KimoApp, pos: egui::Pos2) {
        click_at_sized(ctx, app, pos, 1400.0);
    }

    /// A click in a window of the width given: the popup layout's tests click
    /// in a window the column layout never sees.
    fn click_at_sized(ctx: &egui::Context, app: &mut KimoApp, pos: egui::Pos2, width: f32) {
        run_app_frame_sized(
            ctx,
            app,
            vec![egui::Event::PointerMoved(pos)],
            egui::vec2(width, 900.0),
        );
        for pressed in [true, false] {
            run_app_frame_sized(
                ctx,
                app,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                }],
                egui::vec2(width, 900.0),
            );
        }
    }

    /// The rows a folder becomes, without a frame in the way: one per
    /// conversation, its mails under it once it is open, and one per mail when
    /// the list is not threading.
    #[test]
    fn the_rows_are_one_per_conversation_and_its_mails_once_open() {
        let mails = vec![
            threaded_mail("3", "Re: Hello", "c@x", &["a@x", "b@x"], 300),
            threaded_mail("2", "Re: Hello", "b@x", &["a@x"], 200),
            threaded_mail("1", "Hello", "a@x", &[], 100),
            threaded_mail("4", "Groceries", "d@x", &[], 50),
        ];
        let threads = threads::group(&mails);
        assert_eq!(threads.len(), 2);

        let closed: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert_eq!(
            list_rows_of(&threads, &all_in_order(&mails), &closed, true),
            vec![
                ListRow::Thread { thread: 0 },
                // A conversation of one is a mail, and is drawn like one: no
                // chevron, no indent, nothing to open.
                ListRow::Message {
                    index: 3,
                    in_thread: false
                },
            ]
        );

        // Open: the conversation's own row, then its mails oldest first.
        let open: std::collections::HashSet<String> =
            [threads[0].key.clone()].into_iter().collect();
        assert_eq!(
            list_rows_of(&threads, &all_in_order(&mails), &open, true),
            vec![
                ListRow::Thread { thread: 0 },
                ListRow::Message {
                    index: 2,
                    in_thread: true
                },
                ListRow::Message {
                    index: 1,
                    in_thread: true
                },
                ListRow::Message {
                    index: 0,
                    in_thread: true
                },
                ListRow::Message {
                    index: 3,
                    in_thread: false
                },
            ]
        );

        // Not threading: every mail is a row of its own.
        assert_eq!(
            list_rows_of(&threads, &all_in_order(&mails), &open, false),
            (0..mails.len())
                .map(|index| ListRow::Message {
                    index,
                    in_thread: false
                })
                .collect::<Vec<_>>()
        );
    }

    /// A conversation is one row showing its newest mail under the thread's
    /// subject, with the count of what it holds — and the mails inside it get no
    /// rows of their own until the chevron is clicked.
    fn a_conversation_row(app: &mut KimoApp, ctx: &egui::Context) -> egui::Pos2 {
        let texts = frame_texts(ctx, app);
        assert_eq!(
            texts.iter().filter(|text| *text == "Hello").count(),
            1,
            "one row, under the thread's own subject: {texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text == "Re: Hello"),
            "the mail it holds has no row of its own: {texts:?}"
        );
        assert!(
            texts.iter().any(|text| text == "2"),
            "the row says how many mails it holds: {texts:?}"
        );
        assert!(
            texts.iter().any(|text| text == "preview of 3"),
            "and shows what the newest one says: {texts:?}"
        );
        egui::pos2(300.0, TITLEBAR_HEIGHT + MESSAGE_ROW_HEIGHT / 2.0)
    }

    /// Two read mails of one conversation, and an app showing their folder. Read
    /// so the row draws each line once: an unread one is struck through twice
    /// (`paint_row_text`), which is another test's subject.
    fn app_with_one_conversation(ctx: &egui::Context) -> KimoApp {
        let mut newest = threaded_mail("3", "Re: Hello", "c@example.com", &["a@example.com"], 300);
        newest.seen = true;
        let mut oldest = threaded_mail("1", "Hello", "a@example.com", &[], 100);
        oldest.seen = true;
        app_with_threaded_mail(ctx, vec![newest, oldest])
    }

    /// Clicking a conversation's row opens it in the reading pane — every mail
    /// it holds, oldest first, as cards of their own — and leaves the list
    /// exactly as it was: opening the list's own rows is the chevron's business.
    #[test]
    fn clicking_a_conversation_opens_it_in_the_pane_and_not_in_the_list() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_one_conversation(&ctx);
        let row = a_conversation_row(&mut app, &ctx);
        let before = app.list_rows.as_ref().expect("rows").rows.clone();

        click_at(&ctx, &mut app, row);

        assert_eq!(
            app.selected_message.as_deref(),
            Some("3"),
            "the conversation opens on the mail its row speaks for"
        );
        let rows = app.list_rows.as_ref().expect("rows");
        assert_eq!(rows.rows, before, "the list did not change shape");

        // The pane holds the whole conversation: both mails, in reading order.
        let texts = frame_texts(&ctx, &mut app);
        let oldest = texts
            .iter()
            .position(|text| text == "body of 1")
            .unwrap_or_else(|| panic!("the oldest mail is not on the pane: {texts:?}"));
        let newest = texts
            .iter()
            .position(|text| text == "body of 3")
            .unwrap_or_else(|| panic!("the newest mail is not on the pane: {texts:?}"));
        assert!(
            oldest < newest,
            "a conversation reads oldest first: {texts:?}"
        );
    }

    /// A conversation is one card per mail on the pane, the same card a mail on
    /// its own gets: the pane's own surface, as wide as the pane, with the
    /// chrome showing between two of them. Nothing is nested — no card drawn
    /// around the conversation with a card per mail inside it.
    #[test]
    fn a_conversation_draws_one_card_per_mail_on_the_pane() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_one_conversation(&ctx);
        app.selected_message = Some("3".to_string());
        // A frame to settle the columns: the list's own edge is what tells the
        // pane's cards apart from the surface the list is drawn on.
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
        let cards_of = |app: &mut KimoApp| pane_cards(&ctx, app);

        let cards = cards_of(&mut app);
        assert_eq!(cards.len(), 2, "one card per mail: {cards:?}");
        let (first, second) = (cards[0], cards[1]);
        let pane_left = app.list_column.right();
        for card in &cards {
            assert!(
                card.left() <= pane_left + CONTENT_GUTTER + 0.5
                    && card.right() >= 1400.0 - CONTENT_GUTTER - 0.5,
                "a card spans the pane, gutter to gutter: {card:?}"
            );
        }
        // The first card starts below the floating title bar — the gutter under
        // the bar, like its other sides — so its top edge, and with it the two
        // rounded corners, is on screen rather than behind the glass.
        assert!(
            (first.top() - (TITLEBAR_HEIGHT + CONTENT_GUTTER)).abs() < 0.5,
            "the pane's first card starts under the bar's height: {first:?}"
        );
        let gap = second.top() - first.bottom();
        assert!(
            (gap - CONVERSATION_CARD_GAP).abs() < 0.5,
            "the chrome between two cards is {gap} pt tall"
        );
        assert!(
            !(first.contains_rect(second) || second.contains_rect(first)),
            "the cards are stacked, not one inside another: {cards:?}"
        );
        // The old shape: a card around all of them, and a card per mail inside
        // it — the faint fill and hairline an inset card is drawn with. The
        // pane has neither now; the list's own faint fills are its business.
        let inset = fills_in_color(
            &frame_shapes_at(&ctx, &mut app, 1400.0),
            theme::of(&ctx).card,
        );
        assert!(
            inset.iter().all(|rect| rect.center().x < pane_left),
            "no inset card left in the pane: {inset:?}"
        );

        // And the card a mail on its own gets, with threading off, is the same
        // card: same edges, same width.
        app.threaded = false;
        let alone = cards_of(&mut app);
        assert_eq!(alone.len(), 1, "a mail of its own is one card: {alone:?}");
        assert_eq!(alone[0], first, "a conversation's card is the pane's card");
    }

    /// The chevron is what opens and closes a conversation's mails in the list,
    /// and it does nothing else: no mail is opened by it, and the pane is left
    /// where it was.
    #[test]
    fn the_chevron_opens_and_closes_the_mails_under_a_conversation() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_one_conversation(&ctx);
        let row = a_conversation_row(&mut app, &ctx);
        let chevron = egui::pos2(app.list_column.left() + 12.0, row.y);

        click_at(&ctx, &mut app, chevron);
        // The click is applied at the end of the frame that saw it, so the rows
        // it opened are the next frame's to derive.
        let _ = frame_texts(&ctx, &mut app);
        let rows = app.list_rows.as_ref().expect("rows").rows.clone();
        assert!(
            rows.iter().any(|row| matches!(
                row,
                ListRow::Message {
                    in_thread: true,
                    ..
                }
            )),
            "the mails are under the row now: {rows:?}"
        );
        assert_eq!(
            app.selected_message, None,
            "opening a conversation in the list does not open a mail"
        );

        // The chevron of the first row has not moved, so the same click closes
        // it again.
        click_at(&ctx, &mut app, chevron);
        let _ = frame_texts(&ctx, &mut app);
        let rows = app.list_rows.as_ref().expect("rows").rows.clone();
        assert!(
            rows.iter().all(|row| matches!(row, ListRow::Thread { .. })),
            "the conversation closed again: {rows:?}"
        );
    }

    /// Threading is a preference, and it is the list's shape: turned off, every
    /// mail is a row of its own and nothing is grouped.
    #[test]
    fn the_list_threads_only_when_it_is_asked_to() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mails = vec![
            threaded_mail("2", "Re: Hello", "b@example.com", &["a@example.com"], 200),
            threaded_mail("1", "Hello", "a@example.com", &[], 100),
        ];
        let mut app = app_with_threaded_mail(&ctx, mails.clone());
        app.threaded = false;

        let texts = frame_texts(&ctx, &mut app);
        for subject in ["Hello", "Re: Hello"] {
            assert!(
                texts.iter().any(|text| text == subject),
                "every mail is a row of its own: {texts:?}"
            );
        }
        let rows = app.list_rows.as_ref().expect("the list derived its rows");
        assert_eq!(rows.rows.len(), mails.len());
        assert!(
            rows.rows.iter().all(|row| matches!(
                row,
                ListRow::Message {
                    in_thread: false,
                    ..
                }
            )),
            "a flat list has no conversation rows: {:?}",
            rows.rows
        );
    }

    /// What a card's second line is made of, and the sizes it prints: the type
    /// and the size each appear when the mail was stored with them and are
    /// simply absent when it was not.
    #[test]
    fn a_card_says_what_the_mail_was_stored_with() {
        let full = Attachment {
            name: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size: 68 * 1024,
        };
        assert_eq!(attachment_meta(&full), "application/pdf · 68 KB");
        assert_eq!(
            attachment_meta(&Attachment {
                content_type: String::new(),
                ..full.clone()
            }),
            "68 KB",
            "a size with no type still says something"
        );
        assert_eq!(
            attachment_meta(&Attachment {
                size: 0,
                ..full.clone()
            }),
            "application/pdf"
        );
        assert_eq!(
            attachment_meta(&Attachment {
                name: "old.pdf".to_string(),
                content_type: String::new(),
                size: 0,
            }),
            "",
            "a row from before v6 has nothing to say about itself"
        );

        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1 KB", "a round size loses its .0");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3 GB");
        assert_eq!(human_size(u64::MAX), "16777216 TB", "and it stops at TB");
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

    /// Where a row's checkbox sits: the avatar's slot, `row` rows down the
    /// list. A mail of an open conversation is a step further in with a smaller
    /// circle, and the box follows the slot the circle is in. Select mode's
    /// select-all row takes the top of the list, so every mail row sits one
    /// band lower while the mode is on.
    fn checkbox_center(app: &KimoApp, row: usize, in_thread: bool) -> egui::Pos2 {
        let (indent, diameter) = if in_thread {
            (THREAD_INDENT + THREAD_STEP, THREAD_AVATAR_DIAMETER)
        } else {
            (THREAD_INDENT, AVATAR_DIAMETER)
        };
        let select_all = if app.select_mode {
            SELECT_ALL_HEIGHT
        } else {
            0.0
        };
        egui::pos2(
            app.list_column.left() + LIST_MARGIN as f32 + indent + 8.0 + diameter / 2.0,
            TITLEBAR_HEIGHT + select_all + MESSAGE_ROW_HEIGHT * (row as f32 + 0.5),
        )
    }

    /// Where a row's own flag button sits: the cell just inside the delete one,
    /// so the pair is what the pointer finds at the row's right end. Select
    /// mode's select-all row pushes every mail row one band lower.
    fn row_flag_center(app: &KimoApp, row: usize) -> egui::Pos2 {
        row_delete_center(app, row) - egui::vec2(ROW_ACTION_SIZE + ROW_ACTION_GAP, 0.0)
    }

    /// Where a row's own delete button sits: the cell at the right end of the
    /// row's first line, in the timestamp's slot. Select mode's select-all row
    /// pushes every mail row one band lower.
    fn row_delete_center(app: &KimoApp, row: usize) -> egui::Pos2 {
        let select_all = if app.select_mode {
            SELECT_ALL_HEIGHT
        } else {
            0.0
        };
        egui::pos2(
            app.list_column.right() - LIST_MARGIN as f32 - 10.0 - ROW_ACTION_SIZE / 2.0,
            TITLEBAR_HEIGHT + select_all + MESSAGE_ROW_HEIGHT * row as f32 + ROW_SENDER_Y,
        )
    }

    /// Where the select-all row's own checkbox sits: the plain-row slot in the
    /// band above the first row.
    fn select_all_center(app: &KimoApp) -> egui::Pos2 {
        egui::pos2(
            checkbox_center(app, 0, false).x,
            TITLEBAR_HEIGHT + SELECT_ALL_HEIGHT / 2.0,
        )
    }

    /// The boxes a frame drew in the message list's rows, top to bottom — the
    /// select-all row's own box, if it is there, left out.
    fn row_boxes_in(shapes: &[egui::epaint::ClippedShape], column: egui::Rect) -> Vec<egui::Rect> {
        let mut boxes: Vec<egui::Rect> = checkboxes_in(shapes, column)
            .into_iter()
            .filter(|rect| rect.bottom() > TITLEBAR_HEIGHT + SELECT_ALL_HEIGHT)
            .collect();
        boxes.sort_by(|a, b| a.top().total_cmp(&b.top()));
        boxes
    }

    /// The select-all row's own box, if the frame drew the band: the box the
    /// band puts in the rows' column, in the band's own slot under the title
    /// bar. The band is drawn after the rows and never scrolls, so a box a row
    /// happens to land on that line is the one before it — the last match is the
    /// band's.
    fn select_all_box_in(
        shapes: &[egui::epaint::ClippedShape],
        column: egui::Rect,
    ) -> Option<egui::Rect> {
        let slot = TITLEBAR_HEIGHT + SELECT_ALL_HEIGHT / 2.0;
        checkboxes_in(shapes, column)
            .into_iter()
            .rfind(|rect| (rect.center().y - slot).abs() < 1.0)
    }

    /// The checkboxes a frame drew in the message list, top to bottom: a
    /// `CHECKBOX_SIZE` square with the box's corner radius. Nothing else in the
    /// list is that shape — the hover fill behind a box is wider than the box,
    /// and a row's own edges are hairlines across the whole column.
    fn checkboxes_in(shapes: &[egui::epaint::ClippedShape], column: egui::Rect) -> Vec<egui::Rect> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_checkboxes(&clipped.shape, column, &mut out);
        }
        out
    }

    fn collect_checkboxes(shape: &egui::Shape, column: egui::Rect, out: &mut Vec<egui::Rect>) {
        match shape {
            egui::Shape::Rect(rect) => {
                let size = rect.rect.size();
                if rect.corner_radius == egui::CornerRadius::same(CHECKBOX_CORNER_RADIUS)
                    && (size.x - CHECKBOX_SIZE).abs() < 0.5
                    && (size.y - CHECKBOX_SIZE).abs() < 0.5
                    && column.contains_rect(rect.rect)
                {
                    out.push(rect.rect);
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_checkboxes(shape, column, out);
                }
            }
            _ => {}
        }
    }

    /// The boxes select mode draws are the avatar's slot, and the avatar is what
    /// they replace: a frame without the pointer is all circles, and the pointer
    /// on a row trades exactly that row's circle for a box. Hovering is how the
    /// mode is entered, so the way in is where the mail's mark already is.
    #[test]
    fn the_pointer_puts_a_checkbox_where_the_avatar_was() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);

        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let column = app.list_column;
        assert!(
            checkboxes_in(&shapes, column).is_empty(),
            "the avatars stand while the pointer is away"
        );
        assert_eq!(avatars_in(&shapes, column).len(), 3);

        // The pointer on the second row. It is moved in one frame and the shapes
        // are read from the next: egui hit-tests against the previous frame's
        // rects.
        let at = checkbox_center(&app, 1, false);
        frame_shapes_with(&ctx, &mut app, 1400.0, vec![egui::Event::PointerMoved(at)]);
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let boxes = checkboxes_in(&shapes, column);
        assert_eq!(
            boxes.len(),
            1,
            "one box, over the row under the pointer: {boxes:?}"
        );
        assert_eq!(boxes[0].center(), at, "and it is in the avatar's slot");
        assert_eq!(
            avatars_in(&shapes, column).len(),
            2,
            "the row under the pointer gave its avatar up"
        );
    }

    /// A click on the box turns select mode on and checks the mail the row
    /// stands for. From then on the boxes are not the pointer's business: every
    /// row in the list carries one, and the avatars are gone with the mode.
    #[test]
    fn clicking_a_box_enters_select_mode_with_a_box_on_every_row() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
        let column = app.list_column;

        let at = checkbox_center(&app, 1, false);
        click_at(&ctx, &mut app, at);
        assert!(app.select_mode, "a click on a box is the way into the mode");
        assert!(
            app.selected_mails
                .contains(&MailKey::new("acct", "INBOX", "1")),
            "the box checks the mail its row stands for"
        );
        assert_eq!(
            app.selected_message, None,
            "and a click on the box is not a click on the mail"
        );

        // A second box adds to the selection rather than replacing it — which is
        // the whole point of the mode — and a click on a checked one takes that
        // mail out again without leaving the mode.
        let second = checkbox_center(&app, 2, false);
        click_at(&ctx, &mut app, second);
        assert_eq!(app.selected_mails.len(), 2, "two mails are checked now");
        assert!(
            app.selected_mails
                .contains(&MailKey::new("acct", "INBOX", "2")),
            "the box that was clicked is the mail that joined"
        );
        click_at(&ctx, &mut app, second);
        assert_eq!(
            app.selected_mails.len(),
            1,
            "and clicking it again unchecks it"
        );
        assert!(app.select_mode, "unchecking does not leave the mode");

        // Away from the rows, so what is drawn is the mode and not the hover.
        frame_shapes_with(
            &ctx,
            &mut app,
            1400.0,
            vec![egui::Event::PointerMoved(egui::pos2(1000.0, 500.0))],
        );
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        assert_eq!(row_boxes_in(&shapes, column).len(), 3, "one box per row");
        assert!(
            avatars_in(&shapes, column).is_empty(),
            "select mode has no avatars at all"
        );
        // Two filled boxes: the checked mail's, and the select-all row's, which
        // stands for the one mail of three that is in.
        assert_eq!(
            fills_in_color(&shapes, theme::of(&ctx).accent).len(),
            2,
            "the checked mail's box and the band's"
        );
    }

    /// In select mode the row itself is not the mail any more: a click on it —
    /// anywhere but the box — is the way out of the mode, and it does not open
    /// the mail under the pointer or move the reading pane either.
    #[test]
    fn a_click_on_the_row_leaves_select_mode_without_opening_the_mail() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        app.selected_message = Some("2".to_string());
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

        let at = checkbox_center(&app, 1, false);
        click_at(&ctx, &mut app, at);
        assert!(app.select_mode);
        assert!(!app.selected_mails.is_empty());

        // The row beside the box — the mail's text, well clear of both the box
        // and the select-all row the mode put above it.
        let row = checkbox_center(&app, 1, false);
        click_at(&ctx, &mut app, egui::pos2(320.0, row.y));
        assert!(!app.select_mode, "a click beside the box is the way out");
        assert!(app.selected_mails.is_empty(), "and the checks go with it");
        assert_eq!(
            app.selected_message.as_deref(),
            Some("2"),
            "the row's click opened nothing"
        );
    }

    /// The marks a frame drew inside a box, as the number of points each has: the
    /// check on a checked box is one line of three points, and the dash of a
    /// partly checked conversation a single two-point segment.
    fn strokes_in(shapes: &[egui::epaint::ClippedShape], rect: egui::Rect) -> Vec<usize> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_strokes(&clipped.shape, rect, &mut out);
        }
        out
    }

    fn collect_strokes(shape: &egui::Shape, rect: egui::Rect, out: &mut Vec<usize>) {
        match shape {
            egui::Shape::LineSegment { points, .. } => {
                let bounds = egui::Rect::from_two_pos(points[0], points[1]);
                if rect.contains_rect(bounds) {
                    out.push(2);
                }
            }
            egui::Shape::Path(path) if path.fill == egui::Color32::TRANSPARENT => {
                let bounds = egui::Rect::from_points(&path.points);
                if rect.contains_rect(bounds) {
                    out.push(path.points.len());
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_strokes(shape, rect, out);
                }
            }
            _ => {}
        }
    }

    /// A box says *what* it holds and not only that it holds something: a checked
    /// box is the accent fill with the check drawn on it, and a conversation with
    /// only part of its mail checked gets the dash instead. The two are different
    /// drawings, which is what keeps a half-checked thread from reading as a
    /// checked one.
    #[test]
    fn a_partly_checked_conversation_draws_a_dash_instead_of_a_check() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_one_conversation(&ctx);
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
        // Open the conversation, then check its oldest mail alone: the
        // conversation's own box is the one left in between.
        let key = app
            .list_rows
            .as_ref()
            .expect("the list derived its rows")
            .threads[0]
            .key
            .clone();
        app.open_threads.insert(key);
        app.open_rev += 1;
        app.select_mode = true;
        app.selected_mails
            .insert(MailKey::new("acct", "INBOX", "1"));

        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let column = app.list_column;
        let mut boxes = checkboxes_in(&shapes, column);
        boxes.sort_by(|a, b| a.top().total_cmp(&b.top()));
        assert_eq!(
            boxes.len(),
            4,
            "the select-all row, the conversation and its two mails: {boxes:?}"
        );

        // The band is the first box, and it is partial for the same reason the
        // conversation's is: one mail of the two is checked.
        let band = strokes_in(&shapes, boxes[0]);
        assert_eq!(
            band,
            vec![2],
            "the select-all row says some of the list is in: {band:?}"
        );
        let conversation = strokes_in(&shapes, boxes[1]);
        assert_eq!(
            conversation,
            vec![2],
            "a partly checked conversation draws the dash: {conversation:?}"
        );
        let checked = strokes_in(&shapes, boxes[2]);
        assert_eq!(
            checked,
            vec![3],
            "a checked mail draws the check: {checked:?}"
        );
        assert!(
            strokes_in(&shapes, boxes[3]).is_empty(),
            "and an unchecked one draws nothing: {:?}",
            strokes_in(&shapes, boxes[3])
        );
    }

    /// A checked row is filled the way the reading pane's own mail is: the box is
    /// the precise mark, and the fill is what says at a glance how much of the
    /// list a Delete is about. A half-checked conversation's own row is not
    /// filled — the dash is what says it is partly in.
    #[test]
    fn a_checked_row_takes_the_selection_fill() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        app.select_mode = true;
        app.selected_mails
            .insert(MailKey::new("acct", "INBOX", "1"));

        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let selection = theme::of(&ctx).selection;
        let filled: Vec<egui::Rect> = fills_in_color(&shapes, selection)
            .into_iter()
            .filter(|rect| rect.height() >= MESSAGE_ROW_HEIGHT)
            .collect();
        assert_eq!(filled.len(), 1, "one checked row, one fill: {filled:?}");
        let row = filled[0];
        assert_eq!(
            row.top(),
            TITLEBAR_HEIGHT + SELECT_ALL_HEIGHT + MESSAGE_ROW_HEIGHT,
            "the filled row is the second one — the mail that is checked"
        );
    }

    /// A conversation's row stands for every mail it holds, so its box takes
    /// them all in — and out — at once. Checking one mail of it in the list
    /// under it leaves the conversation's own box saying *some*, which is what
    /// the dash is for.
    #[test]
    fn a_conversations_box_takes_its_whole_thread_in_and_out() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_one_conversation(&ctx);
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

        let at = checkbox_center(&app, 0, false);
        click_at(&ctx, &mut app, at);
        assert_eq!(app.selected_mails.len(), 2, "the whole conversation");
        for uid in ["3", "1"] {
            assert!(
                app.selected_mails
                    .contains(&MailKey::new("acct", "INBOX", uid)),
                "mail {uid} is checked"
            );
        }

        // Open the conversation the way the chevron does: its mails get rows —
        // and boxes — of their own, oldest first under it.
        let key = app
            .list_rows
            .as_ref()
            .expect("the list derived its rows")
            .threads[0]
            .key
            .clone();
        app.open_threads.insert(key);
        app.open_rev += 1;
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
        let at = checkbox_center(&app, 1, true);
        click_at(&ctx, &mut app, at);
        assert_eq!(
            app.selected_mails.len(),
            1,
            "unchecking one mail of the conversation leaves the other"
        );
        assert_eq!(
            app.folder_checkbox(&["3", "1"]),
            Some(CheckState::Partial),
            "the conversation's own box says some of it is checked"
        );
    }

    /// Select mode is the list's business, not the reading pane's: the actions
    /// that are about one mail — reply, reply all, forward, label, flag, export
    /// — are disabled, and Delete, which takes the whole selection, is the one
    /// left live while something is checked.
    #[test]
    fn select_mode_leaves_delete_as_the_only_live_action() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        app.selected_message = Some("0".to_string());
        assert!(
            app.mail_action_enabled(MailAction::Reply),
            "view mode: the pane's mail is what the seven act on"
        );

        app.select_mode = true;
        for action in MailAction::ALL {
            assert!(
                !app.mail_action_enabled(action),
                "nothing is checked yet, so {action:?} has nothing to act on"
            );
        }
        app.selected_mails
            .insert(MailKey::new("acct", "INBOX", "1"));
        for action in MailAction::ALL {
            assert_eq!(
                app.mail_action_enabled(action),
                action == MailAction::Delete,
                "only Delete takes more than one mail: {action:?}"
            );
        }

        // And the bar says which mail or how many: a count is the one thing the
        // seven glyphs cannot show.
        assert_eq!(
            delete_selection_tooltip(1),
            "Delete — removes the selected mail from the server too"
        );
        assert_eq!(
            delete_selection_tooltip(3),
            "Delete — removes the 3 selected mails from the server too"
        );
    }

    /// The bar's own half of that: a disabled cell neither senses clicks nor
    /// fills under the pointer. This puts the pointer on Reply and on Delete in
    /// select mode and reads what each one drew.
    #[test]
    fn the_title_bar_dims_the_one_mail_actions_in_select_mode() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        app.selected_message = Some("0".to_string());
        let actions = mail_action_rect(&ctx, &mut app, 1400.0)
            .expect("the bar gave the actions up at 1400 pt");
        let cell = |action: MailAction| {
            let index = MailAction::ALL
                .iter()
                .position(|candidate| *candidate == action)
                .expect("one of the seven");
            egui::pos2(
                actions.left()
                    + index as f32 * (TITLEBAR_BUTTON_SIZE + TITLEBAR_BUTTON_GAP)
                    + TITLEBAR_BUTTON_SIZE / 2.0,
                TITLEBAR_HEIGHT / 2.0,
            )
        };

        app.select_mode = true;
        app.selected_mails
            .insert(MailKey::new("acct", "INBOX", "1"));
        assert!(
            fill_under_the_pointer(
                &ctx,
                &mut app,
                1400.0,
                cell(MailAction::Reply),
                TITLEBAR_BUTTON_SIZE
            )
            .is_none(),
            "a disabled action does not fill under the pointer"
        );
        assert!(
            fill_under_the_pointer(
                &ctx,
                &mut app,
                1400.0,
                cell(MailAction::Delete),
                TITLEBAR_BUTTON_SIZE
            )
            .is_some(),
            "Delete is the one left live in select mode"
        );
    }

    /// One Delete job per folder: a selection made in a folder is one group, and
    /// one made over a search's hits is a group for every folder they came from,
    /// each in uid order so the same selection is always the same jobs.
    #[test]
    fn the_delete_groups_the_selection_by_folder() {
        let mails: HashSet<MailKey> = [
            MailKey::new("work", "INBOX", "9"),
            MailKey::new("work", "INBOX", "2"),
            MailKey::new("work", "Sent", "5"),
            MailKey::new("home", "INBOX", "1"),
        ]
        .into_iter()
        .collect();

        let batches = delete_batches(&mails);
        assert_eq!(batches.len(), 3, "one job per folder: {batches:?}");
        assert_eq!(
            batches[&("work".to_string(), "INBOX".to_string())],
            vec!["2", "9"]
        );
        assert_eq!(
            batches[&("work".to_string(), "Sent".to_string())],
            vec!["5"]
        );
        assert_eq!(
            batches[&("home".to_string(), "INBOX".to_string())],
            vec!["1"]
        );
    }

    /// A mail the server agreed to delete leaves the selection with its row; the
    /// mode stays, because the next mail is picked from the same list.
    #[test]
    fn a_deleted_mail_leaves_the_selection() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 2);
        app.select_mode = true;
        app.selected_mails
            .insert(MailKey::new("acct", "INBOX", "0"));

        app.apply_event(Event::MessageDeleted {
            account_id: "acct".to_string(),
            folder: "INBOX".to_string(),
            uid: "0".to_string(),
        });

        assert!(
            app.selected_mails.is_empty(),
            "the checked mail went with the row"
        );
        assert!(app.select_mode, "select mode is not what a deletion ends");
    }

    /// A search's hits are selectable too, and a hit names the folder it lives
    /// in: the key its box makes is the hit's own account and folder, not the
    /// column's — the column is not on a folder at all while it shows hits.
    #[test]
    fn a_search_hit_checks_its_own_folder() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.search.text = "report".to_string();
        app.search_hits = vec![SearchHit {
            account_id: "acct".to_string(),
            folder: "Archive".to_string(),
            uid: "7".to_string(),
            subject: "Subject 7".to_string(),
            from: "Ada <ada@example.com>".to_string(),
            to: "me@example.com".to_string(),
            date: "2026-01-01".to_string(),
            timestamp: Some(1_700_000_000),
            seen: false,
            flagged: false,
            attachments: 0,
            snippet: "a line with report in it".to_string(),
        }];
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

        let at = checkbox_center(&app, 0, false);
        click_at(&ctx, &mut app, at);
        assert!(
            app.selected_mails
                .contains(&MailKey::new("acct", "Archive", "7")),
            "the hit's box checks it where it lives: {:?}",
            app.selected_mails
        );
    }

    /// The column moves to another list — another folder, another query — and a
    /// selection is a selection *of* one list: the checks go with it rather than
    /// staying behind for rows that are not on screen any more.
    #[test]
    fn another_folder_or_query_leaves_select_mode() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_folders(&ctx, &[("INBOX", true, 1), ("Archive", true, 1)]);
        app.select_mode = true;
        app.selected_mails
            .insert(MailKey::new("acct", "INBOX", "INBOX-0"));

        app.select_folder(
            "acct".to_string(),
            "Archive".to_string(),
            crate::models::Protocol::Imap,
        );
        assert!(!app.select_mode, "the column shows another folder now");
        assert!(app.selected_mails.is_empty());

        // And the search popup's Clear: the column is the folder again, which is
        // another list than the hits were.
        app.select_mode = true;
        app.selected_mails
            .insert(MailKey::new("acct", "INBOX", "INBOX-0"));
        app.search.text = String::new();
        app.search_applied = None;
        app.pump_search(true);
        assert!(!app.select_mode, "the column shows the folder again");
        assert!(app.selected_mails.is_empty());
    }

    /// The select-all box checks every mail the list holds, not only the rows a
    /// viewport shows: three mails, one click, three checks — and the mode is
    /// still on afterwards.
    #[test]
    fn the_select_all_box_checks_every_mail_in_the_list() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

        // Into the mode through a row's box, then the band's.
        let at = checkbox_center(&app, 1, false);
        click_at(&ctx, &mut app, at);
        let at = select_all_center(&app);
        click_at(&ctx, &mut app, at);

        assert_eq!(app.selected_mails.len(), 3, "every mail of the folder");
        for uid in ["0", "1", "2"] {
            assert!(
                app.selected_mails
                    .contains(&MailKey::new("acct", "INBOX", uid)),
                "mail {uid} is checked"
            );
        }
        assert!(app.select_mode, "the band is not the way out of the mode");
        assert!(
            app.mail_action_enabled(MailAction::Delete),
            "a select-all is exactly what Delete is for"
        );
        assert_eq!(
            app.folder_checkbox(&["0", "1", "2"]),
            Some(CheckState::On),
            "every row's box says so too"
        );
    }

    /// A second click takes a full selection out again — the label has said
    /// Clear selection — and the mode stays where it is, the way unchecking the
    /// last mail by hand does.
    #[test]
    fn the_select_all_box_clears_the_whole_selection() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        app.select_mode = true;
        for uid in ["0", "1", "2"] {
            app.selected_mails
                .insert(MailKey::new("acct", "INBOX", uid));
        }
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

        let at = select_all_center(&app);
        click_at(&ctx, &mut app, at);
        assert!(app.selected_mails.is_empty(), "nothing is checked any more");
        assert!(app.select_mode, "clearing is not leaving");
        assert!(!app.mail_action_enabled(MailAction::Delete));

        // The band's box is an outline again, and no box is filled.
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let band = select_all_box_in(&shapes, app.list_column).expect("the band's box");
        assert!(
            strokes_in(&shapes, band).is_empty(),
            "an empty box has no mark"
        );
        assert!(
            fills_in_color(&shapes, theme::of(&ctx).accent).is_empty(),
            "and nothing in the list is filled"
        );
    }

    /// The band says what is in and what a click would do: an outline with
    /// Select all while nothing is checked, the dash while some of it is, and
    /// the check with Clear selection once it all is.
    #[test]
    fn the_select_all_box_shows_off_partial_and_on() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        app.select_mode = true;

        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let band = select_all_box_in(&shapes, app.list_column).expect("the band's box");
        assert!(
            strokes_in(&shapes, band).is_empty(),
            "nothing checked, no mark"
        );
        assert!(fills_in_color(&shapes, theme::of(&ctx).accent).is_empty());
        assert!(frame_texts(&ctx, &mut app)
            .iter()
            .any(|text| text == "Select all"));

        app.selected_mails
            .insert(MailKey::new("acct", "INBOX", "1"));
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let band = select_all_box_in(&shapes, app.list_column).expect("the band's box");
        assert_eq!(
            strokes_in(&shapes, band),
            vec![2],
            "one mail of three is in, so the band draws the dash"
        );

        for uid in ["0", "2"] {
            app.selected_mails
                .insert(MailKey::new("acct", "INBOX", uid));
        }
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let band = select_all_box_in(&shapes, app.list_column).expect("the band's box");
        assert_eq!(
            strokes_in(&shapes, band),
            vec![3],
            "all of them in, so the band draws the check"
        );
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts.iter().any(|text| text == "Clear selection"),
            "the label turned into the way out: {texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text == "Select all"),
            "and it no longer offers to select what is already in: {texts:?}"
        );
    }

    /// A conversation's row stands for every mail it holds, and the list draws
    /// one row for a closed one — but the select-all box is about the list, not
    /// about the rows, so both mails of the thread are checked by it.
    #[test]
    fn select_all_reaches_mail_inside_a_closed_conversation() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_one_conversation(&ctx);
        app.select_mode = true;
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
        assert_eq!(
            app.list_rows.as_ref().expect("rows").rows.len(),
            1,
            "the conversation is one row while it is closed"
        );

        let at = select_all_center(&app);
        click_at(&ctx, &mut app, at);
        assert_eq!(app.selected_mails.len(), 2, "both mails of the thread");
        for uid in ["3", "1"] {
            assert!(
                app.selected_mails
                    .contains(&MailKey::new("acct", "INBOX", uid)),
                "mail {uid} is checked"
            );
        }
    }

    /// The search results have the row too, and a hit names the folder it lives
    /// in: selecting them all keys them where they live, not in the column.
    #[test]
    fn select_all_in_the_search_results_checks_every_hit() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.search.text = "report".to_string();
        app.search_hits = vec![
            SearchHit {
                account_id: "acct".to_string(),
                folder: "INBOX".to_string(),
                uid: "0".to_string(),
                subject: "Subject 0".to_string(),
                from: "Ada <ada@example.com>".to_string(),
                date: "2026-01-01".to_string(),
                timestamp: Some(1_700_000_000),
                seen: false,
                flagged: false,
                to: "me@example.com".to_string(),
                attachments: 0,
                snippet: "a line with report in it".to_string(),
            },
            SearchHit {
                account_id: "acct".to_string(),
                folder: "Archive".to_string(),
                uid: "7".to_string(),
                subject: "Subject 7".to_string(),
                from: "Ada <ada@example.com>".to_string(),
                date: "2026-01-01".to_string(),
                timestamp: Some(1_700_000_000),
                seen: false,
                flagged: false,
                to: "me@example.com".to_string(),
                attachments: 0,
                snippet: "another line with report in it".to_string(),
            },
            SearchHit {
                account_id: "home".to_string(),
                folder: "INBOX".to_string(),
                uid: "2".to_string(),
                subject: "Subject 2".to_string(),
                from: "Ada <ada@example.com>".to_string(),
                date: "2026-01-01".to_string(),
                timestamp: Some(1_700_000_000),
                seen: false,
                flagged: false,
                to: "me@example.com".to_string(),
                attachments: 0,
                snippet: "a third line with report in it".to_string(),
            },
        ];
        app.select_mode = true;
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

        let at = select_all_center(&app);
        click_at(&ctx, &mut app, at);
        assert_eq!(app.selected_mails.len(), 3, "every hit, across folders");
        for (account, folder, uid) in [
            ("acct", "INBOX", "0"),
            ("acct", "Archive", "7"),
            ("home", "INBOX", "2"),
        ] {
            assert!(
                app.selected_mails
                    .contains(&MailKey::new(account, folder, uid)),
                "{account}/{folder}/{uid} is checked"
            );
        }
    }

    /// The band is the mode's own row, not the list header the column
    /// deliberately has none of: view mode draws no box and no label for it,
    /// and select mode draws exactly one more box than the rows have.
    #[test]
    fn the_select_all_row_is_only_drawn_in_select_mode() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let column = app.list_column;
        assert!(
            select_all_box_in(&shapes, column).is_none(),
            "view mode has no select-all row"
        );
        assert_eq!(
            avatars_in(&shapes, column).len(),
            3,
            "and the avatars stand"
        );
        assert!(!frame_texts(&ctx, &mut app)
            .iter()
            .any(|text| text == "Select all"));

        app.select_mode = true;
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        assert!(
            select_all_box_in(&shapes, column).is_some(),
            "the band's box"
        );
        assert_eq!(row_boxes_in(&shapes, column).len(), 3, "one per row, still");
        assert!(
            avatars_in(&shapes, column).is_empty(),
            "and the avatars are gone with the mode"
        );
    }

    /// The band's box is the rows' box: the same column, one line above the
    /// first mail — which is what makes it read as the header of that column
    /// rather than as a control beside it.
    #[test]
    fn the_select_all_box_lines_up_with_the_row_boxes() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 2);
        app.select_mode = true;

        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let column = app.list_column;
        let band = select_all_box_in(&shapes, column).expect("the band's box");
        let rows = row_boxes_in(&shapes, column);
        assert_eq!(rows.len(), 2, "one box per mail: {rows:?}");
        assert_eq!(band.center().x, rows[0].center().x, "one column of boxes");
        assert_eq!(
            band.center(),
            select_all_center(&app),
            "the band's own slot"
        );
    }

    /// Checking everything and then unchecking one mail by hand leaves the band
    /// saying *some* of the list is in — the same dash a half-checked
    /// conversation gets.
    #[test]
    fn unchecking_one_mail_leaves_the_select_all_box_partial() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        app.select_mode = true;
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

        let at = select_all_center(&app);
        click_at(&ctx, &mut app, at);
        assert_eq!(app.selected_mails.len(), 3);

        let at = checkbox_center(&app, 1, false);
        click_at(&ctx, &mut app, at);
        assert_eq!(app.selected_mails.len(), 2, "one mail was taken out");
        assert_eq!(
            app.folder_checkbox(&["0", "1", "2"]),
            Some(CheckState::Partial),
            "the row's box still says so, and"
        );
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let band = select_all_box_in(&shapes, app.list_column).expect("the band's box");
        assert_eq!(
            strokes_in(&shapes, band),
            vec![2],
            "the band draws the dash for the rest"
        );
    }

    /// What the band's box says about the selection and the list, on its own:
    /// nothing in is off, everything in is on, and the size comparison is the
    /// whole of it.
    #[test]
    fn the_select_all_state_compares_the_selection_with_the_list() {
        assert_eq!(select_all_state(0, 0), CheckState::Off, "an empty list");
        assert_eq!(select_all_state(0, 3), CheckState::Off);
        assert_eq!(select_all_state(1, 3), CheckState::Partial);
        assert_eq!(select_all_state(3, 3), CheckState::On);
    }

    /// The label is the action a click would take, and the count — the one
    /// thing a one-line row cannot show — is in the hover text.
    #[test]
    fn the_select_all_tooltip_counts_the_mails() {
        assert_eq!(select_all_label(CheckState::Off), "Select all");
        assert_eq!(select_all_label(CheckState::Partial), "Select all");
        assert_eq!(select_all_label(CheckState::On), "Clear selection");

        assert_eq!(
            select_all_tooltip(CheckState::Off, 1),
            "Select the mail in this list"
        );
        assert_eq!(
            select_all_tooltip(CheckState::Partial, 3),
            "Select all 3 mails in this list"
        );
        assert_eq!(
            select_all_tooltip(CheckState::On, 1),
            "Clear the selection — the mail in this list is checked"
        );
        assert_eq!(
            select_all_tooltip(CheckState::On, 3),
            "Clear the selection — all 3 mails in this list are checked"
        );
    }

    /// A fresh answer is the whole of what the column shows, so a checked hit
    /// the answer no longer names is dropped with it — the band counts the
    /// selection against the hits and must not be able to count a mail that is
    /// not there. An answer for a query already cleared says nothing about a
    /// folder's checks.
    #[test]
    fn a_new_search_drops_checks_the_hits_no_longer_hold() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 2);
        app.search.text = "report".to_string();
        app.select_mode = true;
        for uid in ["0", "1"] {
            app.selected_mails
                .insert(MailKey::new("acct", "INBOX", uid));
        }

        // An answer that names one of the two: the other is not a hit any more.
        app.apply_event(Event::SearchResults {
            seq: app.search_seq,
            hits: vec![SearchHit {
                account_id: "acct".to_string(),
                folder: "INBOX".to_string(),
                uid: "0".to_string(),
                subject: "Subject 0".to_string(),
                from: "Ada <ada@example.com>".to_string(),
                date: "2026-01-01".to_string(),
                timestamp: Some(1_700_000_000),
                seen: false,
                flagged: false,
                to: "me@example.com".to_string(),
                attachments: 0,
                snippet: "a line with report in it".to_string(),
            }],
        });
        assert_eq!(app.selected_mails.len(), 1, "the answer's own hit");
        assert!(app
            .selected_mails
            .contains(&MailKey::new("acct", "INBOX", "0")));

        // Cleared query: the same event describes hits nobody is looking at.
        app.search = SearchQuery::default();
        app.selected_mails
            .insert(MailKey::new("acct", "INBOX", "1"));
        app.apply_event(Event::SearchResults {
            seq: app.search_seq,
            hits: Vec::new(),
        });
        assert_eq!(
            app.selected_mails.len(),
            2,
            "the folder's checks are not the answer's to drop"
        );
    }

    /// A UIDVALIDITY rotation clears the folder's mail because the UIDs are
    /// meaningless; a check made of one is meaningless with it, and the band
    /// counts the selection against the list.
    #[test]
    fn a_rotated_uidvalidity_drops_its_folder_from_the_selection() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.select_mode = true;
        app.selected_mails
            .insert(MailKey::new("acct", "INBOX", "0"));

        app.apply_event(Event::Messages {
            account_id: "acct".to_string(),
            folder: "INBOX".to_string(),
            // The fixture's folder is generation 1: this is a new one.
            uidvalidity: Some(2),
            messages: Vec::new(),
            // A rotation refetches the folder whole; nothing about it is news.
            new_mail: false,
        });

        assert!(
            app.states["acct"].mailbox.folders[0].messages.is_empty(),
            "the folder was cleared"
        );
        assert!(
            app.selected_mails.is_empty(),
            "and the checks of the old UIDs went with it"
        );
    }

    /// The select-all row is the one thing in the column that does not scroll
    /// away: however far down the list is, the box is still under the title bar —
    /// and still the whole list's, not the viewport's.
    #[test]
    fn the_select_all_row_stays_while_the_list_scrolls() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 60);
        app.select_mode = true;
        run_app_frame(&ctx, &mut app, Vec::new());

        // Down the list: the pointer over it, the wheel, then the frames the
        // scroll area's animation needs to come to rest.
        run_app_frame(
            &ctx,
            &mut app,
            vec![egui::Event::PointerMoved(egui::pos2(300.0, 400.0))],
        );
        run_app_frame(
            &ctx,
            &mut app,
            vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -600.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::default(),
            }],
        );
        for _ in 0..30 {
            run_app_frame(&ctx, &mut app, Vec::new());
        }
        assert!(app.list_scroll > 0.0, "the list did not scroll");

        // The band is where it was: pinned under the title bar, over the rows.
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let band = select_all_box_in(&shapes, app.list_column).expect("the band's box");
        assert_eq!(
            band.center(),
            select_all_center(&app),
            "the band scrolled away with the list"
        );

        // And it is still the control it was: a click checks the whole list.
        let at = select_all_center(&app);
        click_at(&ctx, &mut app, at);
        assert_eq!(
            app.selected_mails.len(),
            60,
            "the band checks the folder, not the rows a viewport shows"
        );
    }

    /// A row offers its own delete button under the pointer: the timestamp gives
    /// way to a muted trash glyph, and the glyph takes the scheme's danger fill
    /// once the pointer is on the button itself. Nothing about the row's layout
    /// moves — the sender is elided against the timestamp's slot either way.
    #[test]
    fn the_pointer_puts_a_delete_button_on_the_row_it_is_on() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        let date = {
            let msg = &app.folder_messages_of("acct", "INBOX").expect("mail")[1];
            date_label(msg.timestamp, &msg.date)
        };
        let danger = theme::of(&ctx).danger_hover;

        // The pointer away: every row's timestamp stands — the fixture's mails
        // share one, so the count is what says one of them went — and no button
        // is drawn.
        let texts = frame_texts(&ctx, &mut app);
        assert_eq!(
            texts.iter().filter(|text| *text == &date).count(),
            3,
            "the timestamps stand while the pointer is away: {texts:?}"
        );
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        assert!(
            fills_in_color(&shapes, danger).is_empty(),
            "nothing is filled at rest"
        );

        // The pointer on the row, away from the button: the timestamp gives way
        // and the glyph is drawn in its slot.
        let row = checkbox_center(&app, 1, false);
        let over_row = egui::pos2(320.0, row.y);
        frame_shapes_with(
            &ctx,
            &mut app,
            1400.0,
            vec![egui::Event::PointerMoved(over_row)],
        );
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let texts = frame_texts(&ctx, &mut app);
        assert_eq!(
            texts.iter().filter(|text| *text == &date).count(),
            2,
            "the hovered row's timestamp gave way to the button: {texts:?}"
        );
        let cell = egui::Rect::from_center_size(
            row_delete_center(&app, 1),
            egui::Vec2::splat(ROW_ACTION_SIZE),
        );
        assert_eq!(
            strokes_in(&shapes, cell).len(),
            3,
            "the trash can is three strokes on its slot"
        );
        assert!(
            fills_in_color(&shapes, danger).is_empty(),
            "and it is not filled until the pointer is on the button"
        );

        // The pointer on the button itself: the danger fill under the glyph.
        let at = row_delete_center(&app, 1);
        frame_shapes_with(&ctx, &mut app, 1400.0, vec![egui::Event::PointerMoved(at)]);
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        assert!(
            fills_in_color(&shapes, danger)
                .iter()
                .any(|rect| rect.center() == at),
            "the button takes the danger fill under the pointer"
        );
    }

    /// The button is a target of its own: a click on it is not a click on the
    /// row, so the mail it deletes is not the mail the reading pane opens.
    #[test]
    fn the_row_delete_button_is_not_a_click_on_the_mail() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

        let at = row_delete_center(&app, 1);
        click_at(&ctx, &mut app, at);
        assert_eq!(
            app.selected_message, None,
            "the click deleted rather than opened the mail"
        );
    }

    /// In select mode the row's own button is not the selection's: it takes the
    /// one mail the row speaks for and leaves the mode and the checks alone.
    #[test]
    fn the_row_delete_button_leaves_select_mode_alone() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        app.select_mode = true;
        app.selected_mails
            .insert(MailKey::new("acct", "INBOX", "0"));
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

        let at = row_delete_center(&app, 2);
        click_at(&ctx, &mut app, at);
        assert!(app.select_mode, "the row's button is not the way out");
        assert_eq!(
            app.selected_mails.len(),
            1,
            "the checks are the box's business"
        );
        assert!(
            app.selected_mails
                .contains(&MailKey::new("acct", "INBOX", "0")),
            "and the checked mail is still checked"
        );
        assert_eq!(app.selected_message, None, "and no mail was opened");
    }

    /// A conversation's row speaks for its newest mail — the one the reading pane
    /// opens from it — and that is the mail its own button takes, not the whole
    /// thread: the thread's mails are the box's (all at once) or the title bar's
    /// (one at a time) to delete.
    #[test]
    fn a_conversations_delete_button_takes_the_mail_its_row_speaks_for() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_one_conversation(&ctx);
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

        let rows = app.list_rows.as_ref().expect("the list derived its rows");
        let messages = app.folder_messages_of("acct", "INBOX").expect("mail");
        let conversation = rows.rows[0];
        assert!(
            matches!(conversation, ListRow::Thread { .. }),
            "the fixture's row is a conversation: {:?}",
            conversation
        );
        assert_eq!(
            row_action_target(conversation, rows, messages).map(|msg| msg.uid.as_str()),
            Some("3"),
            "the newest mail of the thread, not the whole of it"
        );

        // A plain mail row takes the mail it is.
        let plain = ListRow::Message {
            index: 1,
            in_thread: false,
        };
        assert_eq!(
            row_action_target(plain, rows, messages).map(|msg| msg.uid.as_str()),
            Some(messages[1].uid.as_str())
        );
    }

    /// What the button says it takes: the same wording as the title bar's Delete
    /// for a mail, and the newest mail named outright for a conversation, since
    /// one click there takes one mail of it.
    #[test]
    fn the_row_delete_button_says_what_it_takes() {
        assert_eq!(
            row_delete_tooltip(RowPlace::Plain),
            MailAction::Delete.tooltip()
        );
        assert_eq!(
            row_delete_tooltip(RowPlace::InThread),
            MailAction::Delete.tooltip()
        );
        let thread = row_delete_tooltip(RowPlace::Thread {
            open: false,
            count: 3,
            key: "1",
        });
        assert!(
            thread.contains("newest mail"),
            "a conversation's button says which mail it takes: {thread}"
        );
    }

    /// The row's other cell: the flag, just inside the delete button. A click
    /// runs the title bar's own flag action on the mail the row speaks for —
    /// `Job::SetFlag` behind it — and is not a click on the row, so a mail is
    /// marked where it is listed rather than opened.
    #[test]
    fn the_row_flag_button_marks_the_mail_it_lists() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
        let scheme = theme::of(&ctx);

        let at = row_flag_center(&app, 1);
        click_at(&ctx, &mut app, at);
        assert_eq!(
            app.selected_message, None,
            "the click flagged rather than opened the mail"
        );
        let messages = app.folder_messages_of("acct", "INBOX").expect("mail");
        let flags: Vec<bool> = messages.iter().map(|mail| mail.flagged).collect();
        assert!(flags[1], "the mail the row lists is flagged: {flags:?}");
        assert!(!flags[0] && !flags[2], "and only that one");

        // The pointer on another row: the flagged row takes the warning fill —
        // the pointer's own fill stands only while it is on *its* row — which is
        // the mark the cell put on the mail.
        let below = checkbox_center(&app, 2, false);
        frame_shapes_with(
            &ctx,
            &mut app,
            1400.0,
            vec![egui::Event::PointerMoved(egui::pos2(320.0, below.y))],
        );
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        assert!(
            fills_in_color(&shapes, scheme.flag)
                .iter()
                .any(|rect| rect.width() > 200.0),
            "the flagged row takes the warning fill"
        );

        // The pointer on the row itself, away from the cells: the flag button is
        // there — the pennant and its pole — muted and unfilled, the way the
        // delete button beside it is.
        let row = checkbox_center(&app, 1, false);
        frame_shapes_with(
            &ctx,
            &mut app,
            1400.0,
            vec![egui::Event::PointerMoved(egui::pos2(320.0, row.y))],
        );
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        assert!(
            fills_of_size(&shapes, egui::Vec2::splat(ROW_ACTION_SIZE))
                .iter()
                .all(|fill| fill.rect.center() != at),
            "the cell is not filled until the pointer is on it"
        );
        let cell = egui::Rect::from_center_size(at, egui::Vec2::splat(ROW_ACTION_SIZE));
        assert_eq!(
            strokes_in(&shapes, cell).len(),
            2,
            "the pennant and its pole stand on the cell"
        );

        // The pointer on the cell: the warning tint under the glyph, which is
        // what the click puts on the mail.
        frame_shapes_with(&ctx, &mut app, 1400.0, vec![egui::Event::PointerMoved(at)]);
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        assert!(
            fills_of_size(&shapes, egui::Vec2::splat(ROW_ACTION_SIZE))
                .iter()
                .any(|fill| fill.rect.center() == at && fill.fill == scheme.flag),
            "the cell takes the warning tint under the pointer"
        );
    }

    /// The same cell takes the mark off a flagged mail, and says so — the title
    /// bar's own wording, which turns over with the state; a conversation's row
    /// names the newest mail it marks, the way its delete button beside it does.
    #[test]
    fn the_row_flag_button_takes_the_mark_off_and_says_which_mail() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        app.set_flag_on_mail(&MailKey::new("acct", "INBOX", "1"), true);
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

        let at = row_flag_center(&app, 1);
        click_at(&ctx, &mut app, at);
        assert!(
            !app.folder_messages_of("acct", "INBOX").expect("mail")[1].flagged,
            "the second click takes the flag off"
        );

        assert_eq!(
            row_flag_tooltip(RowPlace::Plain, false),
            MailAction::tooltip(MailAction::Flag)
        );
        assert_eq!(
            row_flag_tooltip(RowPlace::InThread, true),
            MailAction::flag_tooltip(true),
            "a mail of a thread says what the click takes off"
        );
        let thread = row_flag_tooltip(
            RowPlace::Thread {
                open: false,
                count: 2,
                key: "1",
            },
            false,
        );
        assert!(
            thread.contains("newest mail"),
            "a conversation's cell says which mail it marks: {thread}"
        );
    }

    /// A conversation's row speaks for its newest mail, and its flag cell marks
    /// that one — not the thread: the mails under it are their own rows' to mark.
    #[test]
    fn a_conversations_flag_button_marks_the_mail_its_row_speaks_for() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_one_conversation(&ctx);
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

        let at = row_flag_center(&app, 0);
        click_at(&ctx, &mut app, at);
        let messages = app.folder_messages_of("acct", "INBOX").expect("mail");
        let flagged: Vec<&str> = messages
            .iter()
            .filter(|mail| mail.flagged)
            .map(|mail| mail.uid.as_str())
            .collect();
        assert_eq!(
            flagged,
            ["3"],
            "the newest mail of the thread, and nothing under it"
        );
    }

    /// A hit's own delete button is keyed by the hit and not by the row: the
    /// click is not the row's, so the window does not switch to the folder the
    /// hit lives in.
    #[test]
    fn a_search_hits_delete_button_is_not_its_row() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.search.text = "report".to_string();
        app.search_hits = vec![SearchHit {
            account_id: "acct".to_string(),
            folder: "Archive".to_string(),
            uid: "7".to_string(),
            subject: "Subject 7".to_string(),
            from: "Ada <ada@example.com>".to_string(),
            to: "me@example.com".to_string(),
            date: "2026-01-01".to_string(),
            timestamp: Some(1_700_000_000),
            seen: false,
            flagged: false,
            attachments: 0,
            snippet: "a line with report in it".to_string(),
        }];
        let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

        let at = row_delete_center(&app, 0);
        click_at(&ctx, &mut app, at);
        assert_eq!(
            app.selected_folder.as_deref(),
            Some("INBOX"),
            "the click deleted rather than opened the hit"
        );
        assert_eq!(app.selected_message, None);
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

    /// The box one painted stroke covers, for the tests that look for a glyph
    /// rather than for text. `None` for everything a glyph is not: text, fills,
    /// images, meshes, and nested shape lists, which the caller walks itself.
    fn stroke_bounds(shape: &egui::Shape) -> Option<egui::Rect> {
        match shape {
            egui::Shape::LineSegment { points, .. } => {
                Some(egui::Rect::from_two_pos(points[0], points[1]))
            }
            egui::Shape::QuadraticBezier(quad) => Some(egui::Rect::from_points(&quad.points)),
            // A path only counts when it is a line drawing: the chevron that
            // opens a conversation is a filled polygon, and it is no more a
            // glyph than the avatar's circle is.
            egui::Shape::Path(path) if path.fill == egui::Color32::TRANSPARENT => {
                Some(egui::Rect::from_points(&path.points))
            }
            _ => None,
        }
    }

    /// Every stroke of a frame that is small enough to be a glyph and lands in
    /// the message list — the column's own width, below the floating title bar,
    /// which is where its rows are. In the list that is the paperclip and
    /// nothing else: a row's edges span the column, and the title bar's own
    /// glyphs are above it.
    fn list_glyph_strokes(
        shapes: &[egui::epaint::ClippedShape],
        column: egui::Rect,
    ) -> Vec<egui::Rect> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_glyph_strokes(&clipped.shape, column, &mut out);
        }
        out
    }

    fn collect_glyph_strokes(shape: &egui::Shape, column: egui::Rect, out: &mut Vec<egui::Rect>) {
        if let egui::Shape::Vec(shapes) = shape {
            for shape in shapes {
                collect_glyph_strokes(shape, column, out);
            }
            return;
        }
        let Some(bounds) = stroke_bounds(shape) else {
            return;
        };
        let centre = bounds.center();
        if bounds.width() <= ROW_CLIP_SIZE + 2.0
            && bounds.height() <= ROW_CLIP_SIZE + 2.0
            && centre.x > column.left()
            && centre.x < column.right()
            && centre.y > TITLEBAR_HEIGHT
        {
            out.push(bounds);
        }
    }

    /// Where every straight stroke the frame drew is centred. A cross drawn by
    /// `paint_close_icon` is two strokes whose midpoints are the same point, so
    /// a button of that shape — and only that shape — is found by asking for it
    /// rather than by working out where it ought to be.
    fn stroke_centres(shapes: &[egui::epaint::ClippedShape], area: egui::Rect) -> Vec<egui::Pos2> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_stroke_centres(&clipped.shape, area, &mut out);
        }
        out
    }

    fn collect_stroke_centres(shape: &egui::Shape, area: egui::Rect, out: &mut Vec<egui::Pos2>) {
        match shape {
            egui::Shape::LineSegment { points, .. } => {
                let centre = egui::pos2(
                    (points[0].x + points[1].x) / 2.0,
                    (points[0].y + points[1].y) / 2.0,
                );
                if area.contains(centre) {
                    out.push(centre);
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_stroke_centres(shape, area, out);
                }
            }
            _ => {}
        }
    }

    /// The rects of the attachment cards a frame drew: a card's chrome is a
    /// rounded rect exactly one card high with a border on it.
    fn attachment_card_rects(
        shapes: &[egui::epaint::ClippedShape],
        area: egui::Rect,
    ) -> Vec<egui::Rect> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_card_rects(&clipped.shape, area, &mut out);
        }
        out
    }

    fn collect_card_rects(shape: &egui::Shape, area: egui::Rect, out: &mut Vec<egui::Rect>) {
        match shape {
            egui::Shape::Rect(rect) => {
                if rect.stroke.width > 0.0
                    && (rect.rect.height() - ATTACHMENT_CARD_HEIGHT).abs() < 0.5
                    && area.intersects(rect.rect)
                {
                    out.push(rect.rect);
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_card_rects(shape, area, out);
                }
            }
            _ => {}
        }
    }

    /// A dropped file, as the window hands it to egui: what a test drops on the
    /// compose card. The backend owns the real one (`egui::DroppedFile`), and all
    /// this asks of it is where the file is and what is in it.
    #[derive(Debug)]
    struct DroppedFile(std::path::PathBuf);

    impl egui::DroppedFile for DroppedFile {
        fn path(&self) -> &std::path::Path {
            &self.0
        }

        fn bytes(&self) -> Result<Vec<u8>, String> {
            std::fs::read(&self.0).map_err(|error| error.to_string())
        }
    }

    /// Drop files on the window: one frame with them in the input, the way a
    /// file manager hands them over.
    fn drop_files(ctx: &egui::Context, app: &mut KimoApp, paths: Vec<std::path::PathBuf>) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            dropped_files: paths
                .into_iter()
                .map(|path| std::sync::Arc::new(DroppedFile(path)) as egui::DroppedFileHandle)
                .collect(),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| app.ui(ui));
        output.textures_delta.clear();
    }

    /// Every mail shown, in the folder's own order: what the list is derived from
    /// with no filter and no sort of its own, for the tests that drive
    /// `list_rows_of` themselves — the filter's and the sort's own tests go
    /// through the app, which builds the mask and the order from
    /// `ListFilter::matches` and `ListSort::compare`.
    fn all_in_order(mails: &[MailMessage]) -> Vec<usize> {
        (0..mails.len()).collect()
    }

    /// Every rectangle one frame filled in a given color, however deeply egui
    /// nested it, for the tests that count surfaces rather than click them. A
    /// bar or a panel draws its fill the same way, so what the caller asks for
    /// is told apart by where it is, not by its color alone.
    fn fills_in_color(
        shapes: &[egui::epaint::ClippedShape],
        fill: egui::Color32,
    ) -> Vec<egui::Rect> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_fills_in_color(&clipped.shape, fill, &mut out);
        }
        out
    }

    /// The centres of every filled circle a frame drew in this colour: what an
    /// avatar is, and the one shape on a card nothing else draws.
    fn circles_in_color(
        shapes: &[egui::epaint::ClippedShape],
        fill: egui::Color32,
    ) -> Vec<egui::Pos2> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_circles(&clipped.shape, fill, &mut out);
        }
        out
    }

    fn collect_circles(shape: &egui::Shape, fill: egui::Color32, out: &mut Vec<egui::Pos2>) {
        match shape {
            egui::Shape::Circle(circle) if circle.fill == fill => out.push(circle.center),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_circles(shape, fill, out);
                }
            }
            _ => {}
        }
    }

    fn collect_fills_in_color(shape: &egui::Shape, fill: egui::Color32, out: &mut Vec<egui::Rect>) {
        match shape {
            egui::Shape::Rect(rect) if rect.fill == fill => out.push(rect.rect),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_fills_in_color(shape, fill, out);
                }
            }
            _ => {}
        }
    }

    /// The card the message list drew: the content surface inside the list
    /// column. The pane's card is the same surface on the other side of the
    /// splitter, which is what the column test tells the two apart by.
    fn list_card_in(
        shapes: &[egui::epaint::ClippedShape],
        column: egui::Rect,
        content: egui::Color32,
    ) -> Option<egui::Rect> {
        fills_in_color(shapes, content)
            .into_iter()
            .find(|rect| column.contains(rect.center()))
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

    /// What the pointer fills when it is put at `center`: the `size`-square cell
    /// under it, if the surface there drew one — the bar's cells at
    /// `TITLEBAR_BUTTON_SIZE`, a mail card's at `CARD_ACTION_SIZE`. The pointer is
    /// moved in one frame and the shapes are read from the next — egui
    /// hit-tests against the last frame's rects, so a single frame would still
    /// be filling the cell the pointer left.
    fn fill_under_the_pointer(
        ctx: &egui::Context,
        app: &mut KimoApp,
        width: f32,
        center: egui::Pos2,
        size: f32,
    ) -> Option<egui::epaint::RectShape> {
        frame_shapes_with(ctx, app, width, vec![egui::Event::PointerMoved(center)]);
        let shapes = frame_shapes_with(ctx, app, width, Vec::new());
        fills_of_size(&shapes, egui::Vec2::splat(size))
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
            // the sync cell, the cog and the window controls alone.
            for (a, b) in [
                (rects.content, rects.sync),
                (rects.sync, rects.cog),
                (rects.cog, rects.controls),
                (rects.content, rects.controls),
            ] {
                assert!(
                    !a.intersect(b).is_positive(),
                    "{width} pt: {a:?} overlaps {b:?}"
                );
            }
            // And while there is room for them, they are in that order: what
            // the right end needs is the sync cell, its gap, the cog, its gap,
            // the three controls and the margin from the window's edge.
            let right_end = TITLEBAR_RIGHT_MARGIN
                + TITLEBAR_BUTTON_SIZE
                + TITLEBAR_BUTTON_GAP
                + TITLEBAR_BUTTON_SIZE
                + TITLEBAR_BUTTON_GAP
                + titlebar_cells_width(3);
            if bar(width).width() >= right_end {
                assert!(
                    rects.content.right() <= rects.sync.left()
                        && rects.sync.right() <= rects.cog.left()
                        && rects.cog.right() <= rects.controls.left(),
                    "{width} pt: the groups are out of order: {rects:?}"
                );
            }
            // The cells keep their size whatever the bar does: a window too
            // narrow to show them all cuts the ones at its edge off rather
            // than shrinking every control into an unclickable sliver.
            assert_eq!(rects.sync.width(), TITLEBAR_BUTTON_SIZE, "{width} pt");
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
                bar(width).right() - TITLEBAR_RIGHT_MARGIN,
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
    /// rounded square. This walks the whole bar — the left group, the reading
    /// pane's run of mail actions, the sync cell, the cog and the window
    /// controls — and puts the pointer on each cell in turn, which is also what
    /// says every one of them is a real hit target rather than a glyph painted
    /// on the chrome.
    ///
    /// The runs are measured with `titlebar_cells_width`, which is what the
    /// window controls are placed with and the bar draws the mail actions with;
    /// the left group's cells are found from the layout its row makes (the edge
    /// margin, then a cell every pitch), and the run of mail
    /// actions from `titlebar_anchors`, which is where the bar places it.
    #[test]
    fn every_button_in_the_title_bar_is_the_same_cell() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 4);
        // The run is enabled by the mail the reading pane shows.
        app.selected_message = Some("0".to_string());

        // One frame first: the run is the reading pane's own row, and the
        // pane's rect is only known once the columns have been laid out.
        frame_shapes_at(&ctx, &mut app, 1400.0);
        let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, TITLEBAR_HEIGHT));
        let rects = titlebar_rects(bar, 3);
        let pitch = TITLEBAR_BUTTON_SIZE + TITLEBAR_BUTTON_GAP;
        let middle = TITLEBAR_HEIGHT / 2.0;

        // The left group: the nav toggle and compose — the window's own
        // actions, a cell every pitch, the first of them the bar's left margin
        // in from the window's edge. Sync is not one of them any more: it
        // stands with the cog at the right end.
        let first_left = TITLEBAR_LEFT_INSET;
        let mut cells: Vec<egui::Pos2> = (0..2)
            .map(|index| {
                egui::pos2(
                    first_left + index as f32 * pitch + TITLEBAR_BUTTON_SIZE / 2.0,
                    middle,
                )
            })
            .collect();

        // The mail actions: the run is the reading pane's own row, and its
        // leftmost cell starts where the pane does.
        let actions = titlebar_anchors(
            bar,
            first_group_end(),
            app.list_column.left(),
            app.list_column.right(),
            rects.sync.left(),
        )
        .actions
        .expect("the bar gave the actions up at 1400 pt");
        for index in 0..MailAction::ALL.len() {
            cells.push(egui::pos2(
                actions.left() + index as f32 * pitch + TITLEBAR_BUTTON_SIZE / 2.0,
                middle,
            ));
        }

        // The right end: sync, then the cog, then the three window controls,
        // which end `TITLEBAR_RIGHT_MARGIN` short of the window's own edge.
        cells.push(rects.sync.center());
        cells.push(rects.cog.center());
        for index in 0..3 {
            cells.push(egui::pos2(
                rects.controls.right() - TITLEBAR_BUTTON_SIZE / 2.0 - index as f32 * pitch,
                middle,
            ));
        }

        for center in cells {
            let filled =
                fill_under_the_pointer(&ctx, &mut app, 1400.0, center, TITLEBAR_BUTTON_SIZE)
                    .unwrap_or_else(|| panic!("nothing filled under {center:?}"));
            assert_eq!(filled.rect.center(), center);
            assert_eq!(
                filled.corner_radius,
                egui::CornerRadius::same(TITLEBAR_BUTTON_CORNER_RADIUS),
                "{center:?}: the hover fill is not the rounded cell"
            );
        }

        // The run is the width of its cells, and it ends clear of the window
        // controls whatever the window does.
        assert_eq!(actions.width(), titlebar_cells_width(MailAction::ALL.len()));
        assert!(
            actions.right() + MAIL_ACTION_EDGE_GAP <= rects.sync.min.x + 0.5,
            "the run draws {} pt into the bar's right end",
            actions.right() + MAIL_ACTION_EDGE_GAP - rects.sync.min.x
        );
    }

    /// The bar draws no image: it is cells and glyphs, and its leftmost cell is
    /// the navigation toggle, the bar's own margin in from the window's edge.
    #[test]
    fn the_title_bar_draws_no_image() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 2);
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, TITLEBAR_HEIGHT));
        let images = image_rects_of(&shapes, bar);
        assert!(images.is_empty(), "the bar draws no image: {images:?}");

        // The toggle's cell is the first thing in the bar, the margin before it
        // being the bar's own.
        let centre = egui::pos2(
            TITLEBAR_LEFT_INSET + TITLEBAR_BUTTON_SIZE / 2.0,
            TITLEBAR_HEIGHT / 2.0,
        );
        assert!(
            fill_under_the_pointer(&ctx, &mut app, 1400.0, centre, TITLEBAR_BUTTON_SIZE).is_some(),
            "the toggle's cell is the bar's first"
        );
    }

    /// Where the bar's two anchored groups go, for every window shape that
    /// matters: the box is the message list's, the run the reading pane's, both
    /// clamped clear of the first group — and the run is given up rather than
    /// drawn over the list when the pane has no room for it beside the window
    /// controls.
    #[test]
    fn the_bar_anchors_the_list_and_the_pane() {
        let bar = |width: f32| {
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, TITLEBAR_HEIGHT))
        };
        let first_end = first_group_end();
        let width = titlebar_cells_width(MailAction::ALL.len());
        // What the bar's right end takes: the three window controls, the cog and
        // the sync cell, with the gaps between them and the edge margin.
        let cluster_left = |window: f32| titlebar_rects(bar(window), 3).sync.left();
        // What that right end takes off a window's width, so the case below can
        // build a bar out of the room the box needs rather than a hand-tuned
        // number that has to follow every size the bar's left group grows by.
        let right_end = 1400.0 - cluster_left(1400.0);

        // The default window (1400 pt, a 200 pt navigation column and a 460 pt
        // pane): the box is the list's own edge, the run the pane's — and the
        // pane is 6 pt too short to hold the run beside the window controls, so
        // the run steps left over the pane's edge by less than the card's own
        // margin rather than leaving the bar.
        let anchors = titlebar_anchors(bar(1400.0), first_end, 200.0, 940.0, cluster_left(1400.0));
        assert_eq!(anchors.search, 200.0, "the box is the list's left edge");
        let actions = anchors.actions.expect("the run has room at 1400 pt");
        assert_eq!(actions.width(), width);
        assert_eq!(actions.right(), cluster_left(1400.0) - MAIL_ACTION_EDGE_GAP);
        assert!(
            actions.left() >= 940.0 - MAIL_ACTION_OVERHANG,
            "the run hangs {} pt over the pane's edge, further than the card's margin",
            940.0 - actions.left()
        );

        // The filter cell is the box's own companion: a cell's gap to its right,
        // inside the room the box's plan left for it.
        let filter = anchors
            .filter
            .expect("the bar has room for the filter at 1400 pt");
        assert_eq!(anchors.search_width, Some(TITLEBAR_SEARCH_WIDTH));
        assert_eq!(
            filter.left(),
            anchors.search + TITLEBAR_SEARCH_WIDTH + TITLEBAR_BUTTON_GAP
        );
        assert_eq!(filter.width(), TITLEBAR_BUTTON_SIZE);
        assert!(
            filter.right() + TITLEBAR_GROUP_GAP <= cluster_left(1400.0) + 0.5,
            "the filter stays clear of the bar's right end: {filter:?}"
        );
        // And the sort cell follows it, one cell's gap further on: the two are
        // the box's group, in the order they are given up in.
        let sort = anchors
            .sort
            .expect("the bar has room for the sort at 1400 pt");
        assert_eq!(sort.left(), filter.right() + TITLEBAR_BUTTON_GAP);
        assert_eq!(sort.width(), TITLEBAR_BUTTON_SIZE);
        assert!(
            sort.right() + TITLEBAR_GROUP_GAP <= cluster_left(1400.0) + 0.5,
            "the sort stays clear of the bar's right end: {sort:?}"
        );

        // A bar with room for the filter but not for the sort after it: the
        // filter is the one that stays, and the sort is not drawn at all. The
        // bar ends where the filter's own square does — the box's width, a
        // cell's gap and the filter, and a group's gap at either end.
        let cluster = first_end
            + TITLEBAR_GROUP_GAP
            + TITLEBAR_SEARCH_WIDTH
            + TITLEBAR_BUTTON_GAP
            + TITLEBAR_BUTTON_SIZE
            + TITLEBAR_GROUP_GAP;
        let anchors = titlebar_anchors(bar(cluster + right_end), first_end, 0.0, 0.0, cluster);
        assert_eq!(anchors.search_width, Some(TITLEBAR_SEARCH_WIDTH));
        assert!(anchors.filter.is_some(), "the filter still fits");
        assert!(
            anchors.sort.is_none(),
            "and the sort does not: {:?}",
            anchors.sort
        );

        // A bar with room for the box but none for a cell beside it: the box is
        // what is kept — it is the list's own, and its width is planned first —
        // and neither cell is drawn rather than squeezed or drawn over its
        // neighbours. The bar is the box's own width and the two group gaps
        // around it and nothing more: a cell would need a square on top of that.
        let cluster = first_end + TITLEBAR_GROUP_GAP + TITLEBAR_SEARCH_WIDTH + TITLEBAR_GROUP_GAP;
        let anchors = titlebar_anchors(bar(cluster + right_end), first_end, 0.0, 0.0, cluster);
        assert_eq!(anchors.search_width, Some(TITLEBAR_SEARCH_WIDTH));
        assert!(
            anchors.filter.is_none(),
            "no room for a cell beside the box: {:?}",
            anchors.filter
        );
        assert!(anchors.sort.is_none(), "nor for the one after it");

        // A collapsed navigation column puts the list at the window's edge: the
        // box follows it and stops clear of the first group instead of running
        // under it.
        let anchors = titlebar_anchors(bar(1400.0), first_end, 0.0, 940.0, cluster_left(1400.0));
        assert_eq!(anchors.search, first_end + TITLEBAR_GROUP_GAP);

        // A narrow pane: too little room beside the window controls for the run,
        // and too far from the pane for it to be the pane's. The mail's own card
        // head carries the actions there.
        let anchors = titlebar_anchors(
            bar(1400.0),
            first_end,
            200.0,
            1400.0 - CONTENT_MIN_WIDTH,
            cluster_left(1400.0),
        );
        assert!(
            anchors.actions.is_none(),
            "a {} pt pane is no room for the run: {:?}",
            CONTENT_MIN_WIDTH,
            anchors.actions
        );
        assert!(
            anchors.filter.is_some(),
            "the box's companion is not the pane's: it survives where the run cannot"
        );

        // A window whose pane has no room for the run at all (the navigation
        // collapsed, the list at its minimum) still has room for the filter: it
        // is the box's, and it is the last of the middle band to go.
        let anchors = titlebar_anchors(bar(600.0), first_end, 0.0, 240.0, cluster_left(600.0));
        assert!(anchors.actions.is_none());
        assert_eq!(anchors.search_width, Some(TITLEBAR_SEARCH_WIDTH));
        assert!(
            anchors.filter.is_some(),
            "a 600 pt window still holds the box and its filter"
        );

        // The popup layout: the pane is the whole window, so its edge is behind
        // the first group and the run is the first thing after it.
        let anchors = titlebar_anchors(bar(1400.0), first_end, 0.0, 0.0, cluster_left(1400.0));
        assert_eq!(
            anchors.actions.map(|rect| rect.left()),
            Some(first_end + TITLEBAR_GROUP_GAP)
        );
        assert_eq!(anchors.search, first_end + TITLEBAR_GROUP_GAP);

        // And a window too narrow for the run and the controls together: at
        // 500 pt the sync cell starts at 306, and the run needs 266 pt of the
        // 174 that are left between the first group and that cell.
        assert!(
            titlebar_anchors(bar(500.0), first_end, 0.0, 0.0, cluster_left(500.0))
                .actions
                .is_none(),
            "a 500 pt window has no room for the run beside the controls"
        );
        // Nothing ever overlaps the first group, whatever the anchors were
        // handed — including a pane whose edge is left of it.
        for pane_left in [0.0, 40.0, 104.0, 200.0, 900.0, 1300.0] {
            let anchors =
                titlebar_anchors(bar(1400.0), first_end, 0.0, pane_left, cluster_left(1400.0));
            assert!(anchors.search >= first_end + TITLEBAR_GROUP_GAP);
            if let Some(filter) = anchors.filter {
                assert!(
                    filter.left() >= first_end + TITLEBAR_GROUP_GAP,
                    "{filter:?}"
                );
            }
            if let Some(sort) = anchors.sort {
                assert!(sort.left() >= first_end + TITLEBAR_GROUP_GAP, "{sort:?}");
            }
            if let Some(actions) = anchors.actions {
                assert!(
                    actions.left() >= first_end + TITLEBAR_GROUP_GAP,
                    "{actions:?}"
                );
                assert!(
                    actions.right() + MAIL_ACTION_EDGE_GAP <= cluster_left(1400.0) + 0.5,
                    "{actions:?}"
                );
            }
        }
    }

    /// The one thing the bar gives up when the room runs short is the search
    /// box's width, and then the box itself: its own width while there is room
    /// for it, its floor while the room is short, nothing below that — and never
    /// narrower for a wider bar, which is what keeps a window being resized from
    /// flickering between two shapes.
    #[test]
    fn the_title_bar_gives_its_room_up_in_one_order() {
        let widest = TITLEBAR_SEARCH_WIDTH;
        let narrowest = TITLEBAR_SEARCH_FLOOR;

        assert_eq!(titlebar_search_width(4000.0), Some(widest));
        assert_eq!(titlebar_search_width(widest), Some(widest));
        assert_eq!(titlebar_search_width(widest - 1.0), Some(widest - 1.0));
        assert_eq!(
            titlebar_search_width(narrowest + 30.0),
            Some(narrowest + 30.0)
        );
        assert_eq!(titlebar_search_width(narrowest), Some(narrowest));
        assert_eq!(titlebar_search_width(narrowest - 1.0), None);

        let mut previous = 0.0;
        for step in 0..6000 {
            let room = step as f32 / 4.0;
            let width = titlebar_search_width(room);
            if let Some(width) = width {
                assert!(
                    (narrowest..=widest).contains(&width),
                    "at {room} pt the box is not a box: {width}"
                );
                assert!(
                    width <= room + 0.01,
                    "at {room} pt the box is wider than the room: {width}"
                );
                assert!(
                    width >= previous,
                    "at {room} pt a wider bar holds a narrower box: {width} after {previous}"
                );
                previous = width;
            } else {
                assert_eq!(previous, 0.0, "at {room} pt the box came back");
            }
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
            // The bar's own right end: the sync cell, the cog and the window
            // controls, from the sync cell's left edge to the window's corner.
            // That is the rect the right group draws through — its cells are
            // `TITLEBAR_BUTTON_SIZE` squares inside it — so a shape clipped to it
            // is the group's own, and anything else that reaches into it is what
            // this catches.
            let right_end =
                egui::Rect::from_min_max(egui::pos2(rects.sync.left(), bar.top()), bar.max);
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
                    visible.right() <= rects.sync.left() + 0.5,
                    "at {width} pt the bar drew {visible:?} into the window controls, which \
                     start at {}: it was drawn through {clip:?}",
                    rects.sync.left(),
                    clip = clipped.clip_rect
                );
            }
        }
    }

    /// The mail actions are the reading pane's, and they wait for a mail:
    /// nothing while the pane is empty — not even dimmed cells — the run the
    /// moment one is open, and nothing again when the pane is too narrow to hold
    /// it beside the window controls, where the mail's own card head is what
    /// carries the actions. Select mode is the exception to the first rule:
    /// Delete takes the selection, so the run is there while something is
    /// checked.
    #[test]
    fn the_mail_actions_are_the_panes_and_wait_for_a_mail() {
        /// Enough frames for the run's fade (`MAIL_ACTION_FADE_TIME`) to have
        /// finished at the `1/60` s a headless frame advances by.
        const FADE_FRAMES: usize = 16;

        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        let pitch = TITLEBAR_BUTTON_SIZE + TITLEBAR_BUTTON_GAP;
        let cell = |actions: egui::Rect, index: usize| {
            egui::pos2(
                actions.left() + index as f32 * pitch + TITLEBAR_BUTTON_SIZE / 2.0,
                TITLEBAR_HEIGHT / 2.0,
            )
        };
        let index_of = |action: MailAction| {
            MailAction::ALL
                .iter()
                .position(|candidate| *candidate == action)
                .expect("one of the seven")
        };

        // No mail open: the pane has room for the run and the bar draws none of
        // it — the first cell's place is chrome, not a button.
        let actions = mail_action_rect(&ctx, &mut app, 1400.0).expect("the pane has room");
        assert!(
            fill_under_the_pointer(
                &ctx,
                &mut app,
                1400.0,
                cell(actions, 0),
                TITLEBAR_BUTTON_SIZE
            )
            .is_none(),
            "no mail open, no run"
        );

        // A mail open: the run fades in, and it is the bar's own cells.
        app.selected_message = Some("0".to_string());
        for _ in 0..FADE_FRAMES {
            run_app_frame(&ctx, &mut app, Vec::new());
        }
        let filled = fill_under_the_pointer(
            &ctx,
            &mut app,
            1400.0,
            cell(actions, 0),
            TITLEBAR_BUTTON_SIZE,
        )
        .unwrap_or_else(|| panic!("the run is there with a mail open"));
        assert_eq!(filled.rect.center(), cell(actions, 0));

        // A window whose pane has no room for the run beside the window
        // controls — narrow enough that the navigation has collapsed and the
        // list is at its minimum, so the pane is what is left — gives the run
        // up: not one of its cells is drawn where the run would have been.
        const NARROW: f32 = 600.0;
        app.selected_message = None;
        // The navigation column collapses on the way into a window this narrow,
        // and the columns settle over the frames that follow: what the run has
        // to fit into is the plan the window has *after* that, not the one the
        // frame it arrived on was drawn from.
        for _ in 0..FADE_FRAMES {
            run_app_frame_sized(&ctx, &mut app, Vec::new(), egui::vec2(NARROW, 900.0));
        }
        assert!(
            mail_action_rect(&ctx, &mut app, NARROW).is_none(),
            "a {NARROW} pt window has no room for the run beside the controls"
        );
        app.selected_message = Some("0".to_string());
        for _ in 0..FADE_FRAMES {
            run_app_frame_sized(&ctx, &mut app, Vec::new(), egui::vec2(NARROW, 900.0));
        }
        let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(NARROW, TITLEBAR_HEIGHT));
        let rects = titlebar_rects(bar, 3);
        let packed =
            rects.sync.left() - MAIL_ACTION_EDGE_GAP - titlebar_cells_width(MailAction::ALL.len());
        for index in 0..MailAction::ALL.len() {
            let unheld = egui::pos2(
                packed + index as f32 * pitch + TITLEBAR_BUTTON_SIZE / 2.0,
                TITLEBAR_HEIGHT / 2.0,
            );
            assert!(
                fill_under_the_pointer(&ctx, &mut app, NARROW, unheld, TITLEBAR_BUTTON_SIZE)
                    .is_none(),
                "the run is not drawn where the pane cannot hold it: {unheld:?}"
            );
        }
        // The probe asks the right question: the bar's own cells are there at
        // that width, so it is the run that is missing and not the pointer.
        assert!(
            fill_under_the_pointer(
                &ctx,
                &mut app,
                NARROW,
                egui::pos2(
                    TITLEBAR_LEFT_INSET + TITLEBAR_BUTTON_SIZE / 2.0,
                    TITLEBAR_HEIGHT / 2.0
                ),
                TITLEBAR_BUTTON_SIZE
            )
            .is_some(),
            "the first group's cells are still the bar's"
        );

        // Select mode, nothing open in the pane, a mail checked: Delete is what
        // the selection is for, so the run is there — and it is the only cell of
        // the seven that answers.
        app.selected_message = None;
        app.select_mode = true;
        app.selected_mails
            .insert(MailKey::new("acct", "INBOX", "1"));
        for _ in 0..FADE_FRAMES {
            run_app_frame_sized(&ctx, &mut app, Vec::new(), egui::vec2(1400.0, 900.0));
        }
        let actions = mail_action_rect(&ctx, &mut app, 1400.0).expect("the pane has room");
        assert!(
            fill_under_the_pointer(
                &ctx,
                &mut app,
                1400.0,
                cell(actions, index_of(MailAction::Delete)),
                TITLEBAR_BUTTON_SIZE
            )
            .is_some(),
            "Delete is there for the selection"
        );
        assert!(
            fill_under_the_pointer(
                &ctx,
                &mut app,
                1400.0,
                cell(actions, index_of(MailAction::Reply)),
                TITLEBAR_BUTTON_SIZE
            )
            .is_none(),
            "the rest are about the mail the pane shows"
        );
    }

    /// The box the bar plans for and the box it draws are the same box:
    /// `titlebar_search_width`'s answer is what the widget comes to, margins and
    /// all, and it starts at the message list's own left edge. Two pieces of
    /// arithmetic decide that — the plan's constants and egui's
    /// `desired_width` — and they are not the same arithmetic, so this is what
    /// keeps them from drifting. (`desired_width` is the *whole* box: the margins
    /// come out of it, they are not added to it.)
    #[test]
    fn the_search_box_is_as_wide_as_it_was_planned() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 4);

        // A window with room for the whole bar: the box is at its own width.
        frame_shapes_at(&ctx, &mut app, 1400.0);
        let drawn = ctx
            .read_response(egui::Id::new("titlebar-search"))
            .expect("the bar drew no search box at 1400 pt")
            .rect;
        let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, TITLEBAR_HEIGHT));
        let rects = titlebar_rects(bar, 3);
        let pane_left = app.list_column.right();
        let anchors = titlebar_anchors(
            bar,
            first_group_end(),
            app.list_column.left(),
            pane_left,
            rects.sync.left(),
        );
        let next = anchors
            .actions
            .map_or(rects.sync.left(), |actions| actions.left());
        let planned = titlebar_search_width(next - anchors.search - TITLEBAR_GROUP_GAP)
            .expect("the bar planned no box at 1400 pt");
        assert!(
            (drawn.width() - planned).abs() < 0.5,
            "the bar plans a {planned} pt box and drew a {} pt one",
            drawn.width()
        );
        assert!(
            (drawn.left() - anchors.search).abs() < 0.5,
            "the box starts at the list's own edge: {drawn:?} against {}",
            anchors.search
        );
    }

    /// The search box is the message list's: it starts where the list starts and
    /// follows the splitter that moves the list's left edge — and when the
    /// navigation column is collapsed and the list runs to the window's own edge,
    /// it stops clear of the first group instead of running under it.
    #[test]
    fn the_search_box_follows_the_list() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 4);

        // A wider navigation column moves the list's left edge, and the box
        // with it.
        set_panel_width(&ctx, "nav", 260.0);
        run_app_frame(&ctx, &mut app, Vec::new());
        run_app_frame(&ctx, &mut app, Vec::new());
        let drawn = ctx
            .read_response(egui::Id::new("titlebar-search"))
            .expect("the bar drew no search box")
            .rect;
        assert!(
            (drawn.left() - app.list_column.left()).abs() < 1.0,
            "the box is at the list's left edge: {drawn:?} against {:?}",
            app.list_column
        );

        // Collapsed: the navigation slides out, the list takes the room over,
        // and the box follows it — never onto the first group.
        app.nav_collapsed = true;
        for _ in 0..16 {
            run_app_frame(&ctx, &mut app, Vec::new());
            let drawn = ctx
                .read_response(egui::Id::new("titlebar-search"))
                .expect("the bar drew no search box");
            assert!(
                drawn.rect.left() >= first_group_end() + TITLEBAR_GROUP_GAP - 0.5,
                "the box stays clear of the first group: {:?}",
                drawn.rect
            );
        }
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
        let body_width = held_body_width(&app, "INBOX/0").expect("the body was laid out");
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
                held_body_width(&app, "INBOX/0"),
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
        let reflowed = held_body_width(&app, "INBOX/0").expect("the body is still laid out");
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
        // rather than held at the width the old one was laid out for. The
        // first mail keeps its own entry: a conversation shows several at once.
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(laid_out_at(&mut cache, "INBOX/2", first, 320.0), 320.0);
        assert_eq!(cache.entries.len(), 2);
        // The first one is still there, at the width it settled at.
        assert_eq!(laid_out_at(&mut cache, "INBOX/1", first, 300.0), 400.0);
        // Another body under the same uid is a new body: a re-fetch can replace
        // one the pane is already showing.
        assert_eq!(laid_out_at(&mut cache, "INBOX/2", second, 300.0), 300.0);
        assert_eq!(
            cache.entries[1].fingerprint,
            body_fingerprint(second),
            "the entry was parsed again, not left as it was"
        );
        // Switching renderer, or opening a mail with no HTML at all, drops it.
        cache.forget();
        assert!(cache.entries.is_empty());
        assert_eq!(laid_out_at(&mut cache, "INBOX/2", second, 280.0), 280.0);
    }

    /// A conversation is several bodies at once, and the cache is bounded: the
    /// mail that has been off screen longest is the one a new body evicts.
    #[test]
    fn the_body_cache_holds_a_conversation_and_evicts_the_oldest() {
        let ctx = egui::Context::default();
        let start = Instant::now();
        let mut cache = BodyCache::default();
        let html = "<p>Hello</p>";
        let fingerprint = body_fingerprint(html);
        let at = |cache: &mut BodyCache, uid: &str, now| {
            cache.get(&ctx, uid, fingerprint, html, 400.0, now).1
        };

        // Every mail of a conversation is held, not just the last one asked
        // for: the pane walks all of them on every frame.
        for index in 0..BODY_CACHE {
            at(&mut cache, &format!("INBOX/{index}"), start);
        }
        assert_eq!(cache.entries.len(), BODY_CACHE);
        // One more evicts the mail used longest ago — the first, which nothing
        // has asked for since.
        at(&mut cache, "INBOX/extra", start + Duration::from_millis(10));
        assert_eq!(cache.entries.len(), BODY_CACHE);
        assert!(
            !cache
                .entries
                .iter()
                .any(|entry| entry.identity == "INBOX/0"),
            "the oldest entry is the one that went"
        );
        assert!(cache
            .entries
            .iter()
            .any(|entry| entry.identity == "INBOX/extra"));
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

    /// Give one of `app_with_messages`' mails an HTML body, so the pane has a
    /// page for Blitz to draw.
    fn html_body(app: &mut KimoApp, index: usize) {
        app.states
            .get_mut("acct")
            .unwrap()
            .mailbox
            .folders
            .first_mut()
            .unwrap()
            .messages[index]
            .body_html = Some("<p>The mail.</p>".to_string());
    }

    /// A finished Blitz page for a test: one tile, and the page box the test
    /// knows. Answers the tile's texture too, so a frame can be asked where the
    /// page was drawn relative to the paper under it. `fill` is what the mail
    /// "painted" — an opaque grey to stand in for a page, `TRANSPARENT` for a
    /// mail that painted nothing, which is the case the paper exists for.
    fn ready_blitz_page(
        ctx: &egui::Context,
        size: egui::Vec2,
        fill: egui::Color32,
    ) -> (BlitzPage, egui::TextureId) {
        let [r, g, b, a] = fill.to_array();
        let pixels = [r, g, b, a].repeat(4);
        let image = egui::ColorImage::from_rgba_premultiplied([2, 2], &pixels);
        let texture = ctx.load_texture("test-blitz-tile", image, egui::TextureOptions::LINEAR);
        let tile = texture.id();
        let page = BlitzPage::Ready(RasterizedPage {
            tiles: vec![RasterTile {
                texture,
                offset: egui::Vec2::ZERO,
                size,
            }],
            size,
            links: Vec::new(),
            truncated: false,
        });
        (page, tile)
    }

    /// Show one mail in Blitz mode with a page already rendered for it, and answer
    /// the frame that drew it. The pane is emptied first so the first frame
    /// records the key this geometry wants (`pending_blitz_key`); the page is
    /// then put under exactly that key, and the second frame draws it instead of
    /// starting a render of its own — which is what lets a test look at a Blitz
    /// pane without running the engine.
    fn blitz_frame_with_page(
        ctx: &egui::Context,
        app: &mut KimoApp,
        uid: &str,
        size: egui::Vec2,
    ) -> (Vec<egui::epaint::ClippedShape>, egui::TextureId) {
        app.html_renderer = HtmlRenderer::Blitz;
        app.selected_message = Some(uid.to_string());
        app.blitz_pages.clear();
        app.blitz_order.clear();
        app.pending_blitz_key = None;
        frame_shapes_at(ctx, app, 1400.0);
        let (key, _) = app
            .pending_blitz_key
            .clone()
            .expect("the pane asks for a render of this mail");
        let (page, tile) = ready_blitz_page(ctx, size, egui::Color32::from_gray(128));
        app.blitz_pages.insert(key, page);
        (frame_shapes_at(ctx, app, 1400.0), tile)
    }

    /// True when a frame filled a rectangle of exactly the page's size in this
    /// colour: the paper the Blitz pane drew under the mail.
    fn paper_fill(
        shapes: &[egui::epaint::ClippedShape],
        size: egui::Vec2,
        color: egui::Color32,
    ) -> bool {
        fills_in_color(shapes, color)
            .into_iter()
            .any(|rect| (rect.size() - size).length() < 0.5)
    }

    /// Every shape a frame drew, depth first, in the order it drew them: what the
    /// paper test needs in order to ask whether the paper went under the page.
    fn shapes_in_order(shapes: &[egui::epaint::ClippedShape]) -> Vec<egui::Shape> {
        fn walk(shape: &egui::Shape, out: &mut Vec<egui::Shape>) {
            match shape {
                egui::Shape::Vec(inner) => {
                    for shape in inner {
                        walk(shape, out);
                    }
                }
                other => out.push(other.clone()),
            }
        }
        let mut out = Vec::new();
        for clipped in shapes {
            walk(&clipped.shape, &mut out);
        }
        out
    }

    /// A Blitz page is drawn on a paper, and the paper is *under* the mail: white
    /// by default, so a mail written for white paper is readable in either theme
    /// — while a mail that paints a background of its own has it painted across
    /// the whole page by the renderer, over this.
    #[test]
    fn a_blitz_page_is_drawn_on_white_paper_under_the_mail() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        html_body(&mut app, 0);

        let page = egui::vec2(400.0, 200.0);
        let (shapes, tile) = blitz_frame_with_page(&ctx, &mut app, "0", page);
        assert!(
            paper_fill(&shapes, page, egui::Color32::WHITE),
            "the page is drawn on white paper"
        );

        let ordered = shapes_in_order(&shapes);
        let paper = ordered.iter().position(|shape| {
            matches!(shape, egui::Shape::Rect(rect)
                if rect.fill == egui::Color32::WHITE && (rect.rect.size() - page).length() < 0.5)
        });
        let mail = ordered
            .iter()
            .position(|shape| matches!(shape, egui::Shape::Mesh(mesh) if mesh.texture_id == tile));
        assert!(
            matches!((paper, mail), (Some(paper), Some(mail)) if paper < mail),
            "the paper goes under the page: paper {paper:?}, page {mail:?}"
        );
    }

    /// The paper cell: the sixth of the head's run, and the one control there
    /// that is about the view rather than about the mail. A click flips that one
    /// mail's paper — the mail beside it keeps its own — and costs no render: the
    /// page in the cache is the same page, drawn on the other paper.
    #[test]
    fn the_paper_cell_flips_one_mails_paper_and_nothing_else() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 2);
        html_body(&mut app, 0);
        html_body(&mut app, 1);

        let page = egui::vec2(400.0, 200.0);
        let (shapes, _) = blitz_frame_with_page(&ctx, &mut app, "0", page);
        assert!(paper_fill(&shapes, page, egui::Color32::WHITE));

        let pages = app.blitz_pages.len();
        click_at(&ctx, &mut app, header_action_center(5));
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        assert!(
            paper_fill(&shapes, page, BLITZ_PAPER_DARK),
            "the click drew this mail on the dark surface"
        );
        assert!(
            !paper_fill(&shapes, page, egui::Color32::WHITE),
            "and the white paper is gone"
        );
        assert_eq!(
            app.blitz_pages.len(),
            pages,
            "the flip repainted the frame and started no render"
        );

        // The mail next to it is its own: the paper is per mail.
        let (shapes, _) = blitz_frame_with_page(&ctx, &mut app, "1", page);
        assert!(paper_fill(&shapes, page, egui::Color32::WHITE));

        // And the first keeps what it was given, for the session.
        let (shapes, _) = blitz_frame_with_page(&ctx, &mut app, "0", page);
        assert!(paper_fill(&shapes, page, BLITZ_PAPER_DARK));

        // Clicking again puts it back on the default, which is not a record.
        click_at(&ctx, &mut app, header_action_center(5));
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        assert!(paper_fill(&shapes, page, egui::Color32::WHITE));
        assert!(
            app.blitz_paper.is_empty(),
            "white is the default, not an entry"
        );
    }

    /// The cell is the Blitz page's own: Simple mode draws the mail as widgets on
    /// the card and has no paper to switch, and a plain-text mail has no page at
    /// all — so neither draws the cell.
    #[test]
    fn the_paper_cell_is_only_drawn_for_a_blitz_page() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 2);
        html_body(&mut app, 0);
        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());

        let cell = header_action_center(5);
        assert_eq!(app.html_renderer, HtmlRenderer::Simple);
        assert!(
            fill_under_the_pointer(&ctx, &mut app, 1400.0, cell, CARD_ACTION_SIZE).is_none(),
            "Simple mode draws no paper cell"
        );

        app.html_renderer = HtmlRenderer::Blitz;
        app.selected_message = Some("1".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());
        assert!(
            fill_under_the_pointer(&ctx, &mut app, 1400.0, cell, CARD_ACTION_SIZE).is_none(),
            "a plain-text mail has no page to draw on"
        );

        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());
        assert!(
            fill_under_the_pointer(&ctx, &mut app, 1400.0, cell, CARD_ACTION_SIZE).is_some(),
            "the mail drawn by Blitz carries the cell"
        );
    }

    /// The paper map holds only the mails the reader has changed, and loses an
    /// account's entries with that account's pages.
    #[test]
    fn the_paper_map_holds_only_what_the_reader_changed() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        let key = MailKey::new("acct", "INBOX", "0");

        assert_eq!(app.paper_of(&key), Paper::White, "white is the default");
        app.toggle_blitz_paper(&key);
        assert_eq!(app.paper_of(&key), Paper::Dark);
        assert_eq!(app.blitz_paper.len(), 1);
        app.toggle_blitz_paper(&key);
        assert_eq!(app.paper_of(&key), Paper::White);
        assert!(
            app.blitz_paper.is_empty(),
            "a mail switched back takes its entry with it"
        );

        app.toggle_blitz_paper(&key);
        app.forget_blitz_pages("acct");
        assert!(
            app.blitz_paper.is_empty(),
            "an account's papers go with its pages"
        );

        assert_eq!(Paper::White.color(), egui::Color32::WHITE);
        assert_eq!(Paper::Dark.color(), BLITZ_PAPER_DARK);
        assert_ne!(Paper::White.tooltip(), Paper::Dark.tooltip());
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
            smtp: None,
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
                flagged: false,
                message_id: String::new(),
                in_reply_to: Vec::new(),
                references: Vec::new(),
                body_text: String::new(),
                body_html: None,
                preview: String::new(),
                attachments: vec![],
                inline_images: vec![],
                labels: BTreeSet::new(),
            },
            MailMessage {
                uid: "2".into(),
                subject: String::new(),
                from: String::new(),
                to: String::new(),
                date: String::new(),
                timestamp: Some(200),
                seen: false,
                flagged: false,
                message_id: String::new(),
                in_reply_to: Vec::new(),
                references: Vec::new(),
                body_text: String::new(),
                body_html: None,
                preview: String::new(),
                attachments: vec![],
                inline_images: vec![],
                labels: BTreeSet::new(),
            },
        ];
        sort_messages(&mut msgs);
        assert_eq!(msgs[0].uid, "2");
    }

    /// The Send button is enabled by one rule (`send_blocker`) — and the worker
    /// checks the same one again. No account, an account with no outgoing
    /// server, a sender that is not an address, an entry that is not one, and a
    /// mail with nobody to go to are the reasons a draft cannot leave.
    #[test]
    fn the_send_button_waits_for_an_account_a_sender_and_a_recipient() {
        let can_send = |account: &Account| {
            let mut account = account.clone();
            account.smtp = Some(SmtpSettings {
                host: "smtp.example.com".to_string(),
                port: 587,
                tls: TlsMode::StartTls,
                login: None,
            });
            account
        };
        let bob = ["bob@example.com".to_string()];
        let nobody: [String; 0] = [];
        let junk = ["not an address".to_string()];

        // No account configured at all.
        assert!(send_blocker(None, &bob, &nobody, &nobody).is_some());
        // An account that only reads mail.
        let reading = test_account("imap.example.com");
        assert!(!reading.can_send());
        assert!(send_blocker(Some(&reading), &bob, &nobody, &nobody).is_some());
        // The same account with a way to send, and somebody to send to.
        let ready = can_send(&reading);
        assert_eq!(send_blocker(Some(&ready), &bob, &nobody, &nobody), None);
        // Nobody to send to.
        assert!(send_blocker(Some(&ready), &nobody, &nobody, &nobody).is_some());
        // An entry that is not an address, in any of the three fields.
        assert!(send_blocker(Some(&ready), &junk, &nobody, &nobody).is_some());
        assert!(send_blocker(Some(&ready), &bob, &junk, &nobody).is_some());
        assert!(send_blocker(Some(&ready), &bob, &nobody, &junk).is_some());
        // A username that is not an address cannot be the sender: the From
        // list shows it as one, so the card must not pretend otherwise.
        let mut odd = ready.clone();
        odd.username = "ada".to_string();
        assert!(send_blocker(Some(&odd), &bob, &nobody, &nobody).is_some());
        // The reason names what is wrong: the card shows it verbatim.
        let reason = send_blocker(Some(&ready), &junk, &nobody, &nobody).unwrap();
        assert!(reason.contains("not an address"), "{reason}");
        let unreadable = send_blocker(Some(&reading), &bob, &nobody, &nobody).unwrap();
        assert!(unreadable.contains("outgoing server"), "{unreadable}");
    }

    /// What the notice says about a mail that went out: who it went to, and
    /// what became of the copy in Sent — with the mailbox name decoded for the
    /// reader rather than left in the wire's modified UTF-7.
    #[test]
    fn a_sent_notice_names_the_recipients_and_the_copy() {
        let recipients = [
            "bob@example.com".to_string(),
            "carol@example.com".to_string(),
        ];
        assert_eq!(
            sent_notice(&recipients, &SentCopy::None),
            "Sent to bob@example.com, carol@example.com"
        );
        assert_eq!(
            sent_notice(
                &recipients,
                &SentCopy::Saved {
                    mailbox: "&XfJT0ZAB-".to_string(),
                }
            ),
            "Sent to bob@example.com, carol@example.com · a copy is in 已发送"
        );
        let failed = sent_notice(
            &recipients,
            &SentCopy::Failed("no such mailbox".to_string()),
        );
        assert!(
            failed.ends_with("the copy could not be saved: no such mailbox"),
            "{failed}"
        );
    }

    /// The draft is the only copy of the mail, so the server saying it has the
    /// mail is what closes the card — and a refusal keeps it, with the server's
    /// own words in it, ready to be sent again.
    #[test]
    fn a_sent_mail_closes_the_card_and_a_refusal_keeps_it() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.compose = Some(ComposeDraft::new(Some("acct".to_string())));
        if let Some(draft) = app.compose.as_mut() {
            draft.to = "bob@example.com".to_string();
            draft.subject = "Hello".to_string();
            draft.body = "Body".to_string();
            draft.sending = true;
        }
        // A send in flight: the card says so and its boxes are disabled. The
        // first frame is what sizes a modal's area — the card is drawn from the
        // second one on, which is what the app does live too.
        let _ = frame_texts(&ctx, &mut app);
        let texts = frame_texts(&ctx, &mut app);
        assert!(texts.iter().any(|text| text == "Sending…"), "{texts:?}");

        // The server refused it: the draft stays, the reason is in the card,
        // and Send is enabled again.
        app.apply_event(Event::MessageSendFailed {
            error: "550 5.1.1 no such user".to_string(),
        });
        let draft = app.compose.as_ref().expect("the draft is still there");
        assert!(!draft.sending);
        assert_eq!(draft.error.as_deref(), Some("550 5.1.1 no such user"));
        assert_eq!(draft.to, "bob@example.com", "the draft is untouched");
        let _ = frame_texts(&ctx, &mut app);
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts.iter().any(|text| text.contains("no such user")),
            "{texts:?}"
        );

        // The server has it now: the card goes with the draft, and the line
        // above the message list says where the mail went.
        app.apply_event(Event::MessageSent {
            recipients: vec!["bob@example.com".to_string()],
            copy: SentCopy::Saved {
                mailbox: "Sent".to_string(),
            },
        });
        assert!(app.compose.is_none(), "a sent card is a closed card");
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts
                .iter()
                .any(|text| text == "Sent to bob@example.com · a copy is in Sent"),
            "{texts:?}"
        );
    }

    /// Preferences → Appearance offers the accent as a row of chips — one per
    /// choice, each drawn in the colour it stands for — and the one in force is
    /// ringed in the app's own ink rather than ticked, because a chip *is* a
    /// colour and a mark on it would be a second colour to read.
    #[test]
    fn the_appearance_page_shows_the_accents_with_the_one_in_force_ringed() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.show_preferences = true;
        app.prefs_tab = PrefsTab::Appearance;
        // The key a settings file names is what the app starts with; a click on
        // a chip is the same assignment, and what it writes there is the
        // app's own business rather than this test's.
        app.accent = Accent::parse("teal");
        for _ in 0..FADE_FRAMES {
            run_app_frame(&ctx, &mut app, Vec::new());
        }
        let scheme = theme::of(&ctx);
        let chosen = app.accent.colors(scheme.dark).accent;
        assert_eq!(
            scheme.accent, chosen,
            "the chosen accent is the one the app draws with"
        );

        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let chips: Vec<egui::epaint::RectShape> =
            fills_of_size(&shapes, egui::Vec2::splat(ACCENT_CHIP_SIZE))
                .into_iter()
                .filter(|chip| {
                    Accent::ALL
                        .iter()
                        .any(|accent| accent.colors(scheme.dark).accent == chip.fill)
                })
                .collect();
        assert_eq!(chips.len(), Accent::ALL.len(), "one chip per choice");
        for accent in Accent::ALL {
            let color = accent.colors(scheme.dark).accent;
            assert!(
                chips.iter().any(|chip| chip.fill == color),
                "{} is offered: {:?}",
                accent.name,
                chips.iter().map(|chip| chip.fill).collect::<Vec<_>>()
            );
        }
        // The ring: one stroke, just outside the chip it belongs to — which has
        // to be the chosen one's.
        let ring = ACCENT_CHIP_SIZE + ACCENT_CHIP_RING_GAP * 2.0;
        let rings: Vec<(egui::Rect, egui::Color32)> = stroked_boxes_in(&shapes, popup_area())
            .into_iter()
            .filter(|(rect, _)| {
                (rect.width() - ring).abs() < 0.5 && (rect.height() - ring).abs() < 0.5
            })
            .collect();
        assert_eq!(rings.len(), 1, "one chip is ringed: {rings:?}");
        assert_eq!(rings[0].1, scheme.text, "in the app's own ink");
        let ringed = chips
            .iter()
            .find(|chip| chip.rect.contains(rings[0].0.center()))
            .expect("the ring is round one of the chips");
        assert_eq!(ringed.fill, chosen, "and it is the one in force");
    }

    /// The account form's Sending group: off is an account that only reads
    /// mail, on needs a server and a port, and what it builds is what the
    /// account gets — credentials it does not name included.
    #[test]
    fn the_account_form_builds_the_outgoing_server_it_describes() {
        let mut form = AccountForm::default();
        assert!(!form.is_valid(), "a new form has nothing in it");
        form.name = "Work".to_string();
        form.host = "imap.example.com".to_string();
        form.username = "me@example.com".to_string();
        assert!(form.is_valid(), "reading mail needs no outgoing server");
        assert_eq!(form.to_account("work".to_string()).smtp, None);

        // Switched on with nothing to reach, and then with something.
        form.smtp_enabled = true;
        assert!(!form.is_valid());
        form.smtp_host = "smtp.example.com".to_string();
        assert!(form.is_valid());
        assert_eq!(
            form.to_account("work".to_string()).smtp,
            Some(SmtpSettings {
                host: "smtp.example.com".to_string(),
                port: 465,
                tls: TlsMode::Implicit,
                login: None,
            })
        );

        // A login of its own is the pair below the checkbox, and the security
        // choice is what moves the port.
        form.smtp_tls = TlsMode::StartTls;
        form.smtp_port_str = SmtpSettings::default_port(form.smtp_tls).to_string();
        form.smtp_own_login = false;
        form.smtp_username = "code".to_string();
        form.smtp_password = "abcd efgh".to_string();
        let account = form.to_account("work".to_string());
        assert_eq!(
            account.smtp.as_ref().map(|smtp| smtp.port),
            Some(587),
            "STARTTLS is the submission port"
        );
        assert_eq!(account.smtp_login(), Some(("code", "abcd efgh")));

        // Editing an account fills the group in from what it already has.
        let edit = AccountForm::edit(&account);
        assert!(edit.smtp_enabled);
        assert_eq!(edit.smtp_host, "smtp.example.com");
        assert!(!edit.smtp_own_login);
        assert_eq!(edit.smtp_username, "code");
        assert_eq!(
            edit.smtp(),
            account.smtp,
            "the form describes what the account has"
        );

        // An outgoing server is not part of what makes the cached mail describe
        // another mailbox: moving it must not purge what is stored.
        let mut moved = edit.clone();
        moved.smtp_host = "smtp.other.example".to_string();
        assert!(!moved.points_elsewhere(&account));
        // The incoming endpoint still is.
        let mut elsewhere = edit.clone();
        elsewhere.host = "imap.other.example".to_string();
        assert!(elsewhere.points_elsewhere(&account));
    }

    #[test]
    fn the_result_count_says_when_it_is_a_floor() {
        assert_eq!(results_label(0), "0 results");
        assert_eq!(results_label(1), "1 result");
        assert_eq!(results_label(12), "12 results");
        // At the limit the database stopped asking, so the count is a floor.
        assert_eq!(results_label(SEARCH_LIMIT), "200+ results");
    }

    /// A batch of news is one notification per mail while there are few of
    /// them, and one line counting them once there are many: forty popups is
    /// not something a desktop should be asked to draw.
    #[test]
    fn a_batch_of_new_mail_becomes_one_notification_per_mail_or_a_count() {
        let arrivals = |count: usize| -> Vec<Arrival> {
            (0..count)
                .map(|index| Arrival::of(&test_message(&index.to_string(), false)))
                .collect()
        };

        let notices = new_mail_notices("INBOX", &arrivals(NOTIFY_MAX));
        assert_eq!(notices.len(), NOTIFY_MAX);
        assert_eq!(
            notices[0],
            ("Ada <ada@example.com>".to_string(), "Subject 0".to_string())
        );

        let notices = new_mail_notices("INBOX", &arrivals(NOTIFY_MAX + 1));
        assert_eq!(notices.len(), 1, "one summary instead of four popups");
        assert_eq!(notices[0].0, "4 new messages in INBOX");
        assert_eq!(notices[0].1.lines().count(), NOTIFY_MAX);
        assert!(notices[0].1.starts_with("Ada <ada@example.com>: Subject 0"));
    }

    /// The folders whose new mail is announced and the ones that stay quiet:
    /// the user's own copies, the mailboxes mail is filtered into, and the
    /// ones where a server shows every mail of the account again.
    #[test]
    fn only_delivered_mail_is_announced() {
        for kind in [FolderKind::Inbox, FolderKind::Archive, FolderKind::Folder] {
            assert!(announces_new_mail(kind), "{kind:?} is delivered mail");
        }
        for kind in [
            FolderKind::Sent,
            FolderKind::Drafts,
            FolderKind::Trash,
            FolderKind::Junk,
            FolderKind::AllMail,
            FolderKind::Starred,
        ] {
            assert!(!announces_new_mail(kind), "{kind:?} is not news");
        }
    }

    /// A message the account already holds under another folder is not news a
    /// second time: a server that files one mail in two mailboxes (Gmail's All
    /// Mail, a label beside the Inbox) must not raise two notifications for it.
    #[test]
    fn a_mail_the_account_already_holds_elsewhere_is_not_announced_again() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_folders(&ctx, &[("INBOX", true, 1), ("Receipts", true, 1)]);
        // Each fixture mail carries a Message-ID of its own, so the Inbox's
        // mail — asked about the way a batch that just landed in the Inbox
        // asks — is held nowhere else.
        let inbox_id = app.states["acct"].mailbox.folders[0].messages[0]
            .message_id
            .clone();
        assert!(!app
            .message_ids_elsewhere("acct", "INBOX")
            .contains(inbox_id.as_str()));

        // One mail filed in both — a label beside the Inbox, or Gmail's All
        // Mail — is held elsewhere whichever folder the batch came from, and it
        // is the news of the first of them that is announced.
        app.states.get_mut("acct").unwrap().mailbox.folders[1].messages[0].message_id =
            inbox_id.clone();
        assert!(app
            .message_ids_elsewhere("acct", "INBOX")
            .contains(inbox_id.as_str()));
        assert!(app
            .message_ids_elsewhere("acct", "Receipts")
            .contains(inbox_id.as_str()));

        // An id nobody holds, mail cached before the header was kept (no id at
        // all), and a mail of another account: none of them is in the set.
        let elsewhere = app.message_ids_elsewhere("acct", "INBOX");
        assert!(!elsewhere.contains("nobody@example.com"));
        assert!(!elsewhere.contains(""));
        assert!(app.message_ids_elsewhere("other", "INBOX").is_empty());
    }

    /// One mail to answer, with everything the three composes read out of it:
    /// a sender, two recipients (one of them the user), a dated subject and
    /// a two-line body.
    fn mail_to_answer() -> MailMessage {
        MailMessage {
            uid: "0".to_string(),
            subject: "The report".to_string(),
            from: "Ada <ada@example.com>".to_string(),
            to: "me@example.com, Bob <bob@example.com>".to_string(),
            date: "Tue, 2026-01-01".to_string(),
            timestamp: Some(1_700_000_000),
            seen: false,
            flagged: false,
            message_id: "m0".to_string(),
            in_reply_to: Vec::new(),
            references: Vec::new(),
            body_text: "Here it is.\nSecond line.".to_string(),
            body_html: None,
            preview: "Here it is.".to_string(),
            attachments: Vec::new(),
            inline_images: Vec::new(),
            labels: BTreeSet::new(),
        }
    }

    /// Where a mail's action cells sit, on the pane's first card: the right
    /// end of its head row — the sender's own row, above the subject —
    /// against the card's own padding, delete outermost as the title bar has
    /// it. `from_right` counts cells from the right: 0 delete, 1 label,
    /// 2 forward, 3 reply all, 4 reply — the bar's own run with flag and
    /// export left out.
    fn header_action_center(from_right: usize) -> egui::Pos2 {
        let stride = CARD_ACTION_SIZE + TITLEBAR_BUTTON_GAP;
        let row_height = AVATAR_DIAMETER.max(CARD_ACTION_SIZE);
        egui::pos2(
            1400.0
                - CONTENT_GUTTER
                - CONTENT_PADDING.x
                - CARD_ACTION_SIZE / 2.0
                - stride * from_right as f32,
            TITLEBAR_HEIGHT + CONTENT_GUTTER + CONTENT_PADDING.y + row_height / 2.0,
        )
    }

    /// The three composes that answer a mail are made of it: a reply is
    /// addressed to its sender alone, a reply all to everyone but the user's
    /// own address, a forward to nobody until the sender chooses — and all
    /// three carry the original quoted under the new text, with the subject
    /// prefixed unless it already carries a prefix of its own.
    #[test]
    fn the_answers_to_a_mail_are_made_of_it() {
        let mail = mail_to_answer();
        let quote =
            "On Tue, 2026-01-01, Ada <ada@example.com> wrote:\n> Here it is.\n> Second line.\n";

        let reply = ComposeDraft::reply(&mail, Some("acct".to_string()));
        assert_eq!(reply.kind, ComposeKind::Reply);
        assert_eq!(reply.to, "Ada <ada@example.com>");
        assert_eq!(reply.cc, "");
        assert_eq!(reply.subject, "Re: The report");
        assert_eq!(reply.body, quote);

        let all = ComposeDraft::reply_all(&mail, Some("acct".to_string()), "me@example.com");
        assert_eq!(all.kind, ComposeKind::ReplyAll);
        assert_eq!(all.to, "Ada <ada@example.com>");
        assert_eq!(
            all.cc, "bob@example.com",
            "the user's own address is not sent their own mail"
        );
        assert_eq!(all.subject, "Re: The report");

        let forward = ComposeDraft::forward(&mail, Some("acct".to_string()));
        assert_eq!(forward.kind, ComposeKind::Forward);
        assert_eq!(forward.to, "");
        assert_eq!(forward.cc, "");
        assert_eq!(forward.subject, "Fwd: The report");
        assert_eq!(forward.body, quote);

        // A subject that already answers is not prefixed again — whatever
        // prefix the sender's client wrote.
        assert_eq!(answer_subject("Re", "Re: The report"), "Re: The report");
        assert_eq!(answer_subject("Re", "答复: 报告"), "答复: 报告");
        assert_eq!(answer_subject("Fwd", "The report"), "Fwd: The report");
    }

    /// A mail's own header answers it where the mail is: the reply cell opens
    /// the compose in the pane itself, prefilled from the mail — a card right
    /// under it, so the pane keeps the mail on screen and says both things at
    /// once. The title bar's cell opens the same draft as a popup over
    /// everything.
    #[test]
    fn a_mails_header_answers_it_where_the_mail_is() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());

        // The reply cell, on the mail's header: the card comes up under the
        // mail.
        click_at(&ctx, &mut app, header_action_center(4));
        let draft = app.compose.as_ref().expect("the reply opened a draft");
        assert_eq!(app.compose_spot, ComposeSpot::Pane);
        assert_eq!(draft.kind, ComposeKind::Reply);
        assert_eq!(draft.to, "Ada <ada@example.com>");
        assert_eq!(draft.subject, "Re: Subject 0");
        assert_eq!(
            draft.body,
            "On 2026-01-01, Ada <ada@example.com> wrote:\n> body\n"
        );

        // The pane says both things at once: the mail above, and the card under
        // it — which, being under its own mail, does not say what it is: its
        // first line is who the answer is from, in the place the heading took,
        // and Cc, Bcc and Subject are not among the fields it shows.
        let texts = frame_texts(&ctx, &mut app);
        assert!(texts.iter().any(|text| text == "Subject 0"));
        assert!(
            texts.iter().any(|text| text == "Test <ada>"),
            "the account the answer goes from is on its head bar: {texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text == "Reply") && !texts.iter().any(|text| text == "From"),
            "the place says what this card is, and the head bar's label is a face: {texts:?}"
        );
        for absent in ["Cc", "Bcc", "Subject"] {
            assert!(
                !texts.iter().any(|text| text == absent),
                "an answer written under its mail shows the head bar, To, the files, \
                 the body and the two buttons: {absent} is not one of them, {texts:?}"
            );
        }

        // The head bar's label is the account's own face: a filled circle in the
        // colour its address hashes to, in the label column's place on the card
        // that answers the mail.
        let colour = sender_color(theme::of(&ctx), "ada");
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let answer = *pane_cards(&ctx, &mut app)
            .last()
            .expect("the answer is a card of the pane");
        let avatars: Vec<egui::Pos2> = circles_in_color(&shapes, colour)
            .into_iter()
            .filter(|centre| answer.contains(*centre))
            .collect();
        assert_eq!(
            avatars.len(),
            1,
            "one face on the answer's head bar: {avatars:?} of {answer:?}"
        );

        // The reply's own To is flat beside that face, like the From list
        // above it: the card is a letter, not the popup's form, so the row
        // wears no box — no fill and no outline. The body's own field is the
        // only surface the answer's fields leave on the card, which is what
        // the scheme's input fill counts.
        let fields: Vec<egui::Rect> = fills_in_color(&shapes, theme::of(&ctx).input)
            .into_iter()
            .filter(|rect| answer.contains(rect.center()))
            .collect();
        assert_eq!(
            fields.len(),
            1,
            "only the body is drawn as a box: {fields:?} of {answer:?}"
        );

        // And the footer's two are the card's own: they stand a whole button
        // tall — the stock ones are a line of a form — and one of them, Send,
        // has the plane drawn inside it. (Send is the *disabled* one here: the
        // fixture account has no outgoing server, and a disabled button is the
        // card's own fill rather than the accent.)
        let scheme = theme::of(&ctx);
        let buttons: Vec<egui::Rect> = fills_in_color(&shapes, scheme.card)
            .into_iter()
            .filter(|rect| {
                (rect.height() - COMPOSE_BUTTON_HEIGHT).abs() < 0.5
                    && answer.contains(rect.center())
            })
            .collect();
        assert_eq!(buttons.len(), 2, "Send and Discard: {buttons:?}");
        assert!(
            buttons.iter().all(|rect| rect.width() > 60.0),
            "an icon and a word fit in each: {buttons:?}"
        );
        let planes: Vec<&egui::Rect> = buttons
            .iter()
            .filter(|rect| !glyph_colors_in(&shapes, **rect).is_empty())
            .collect();
        assert_eq!(planes.len(), 1, "the plane is on Send alone: {buttons:?}");

        // The title bar's cell opens the same three as popups — the same
        // draft, another container.
        let mail = app.selected_mail().cloned().expect("the mail to answer");
        app.compose_about(MailAction::ReplyAll, ComposeSpot::Popup, &mail);
        assert_eq!(app.compose_spot, ComposeSpot::Popup);
        assert_eq!(
            app.compose.as_ref().expect("draft").kind,
            ComposeKind::ReplyAll
        );
        // And the popup is the whole form: the same draft in the other container
        // has its heading back, and the three fields the on-the-spot card leaves
        // out — which is where a Reply all's own Cc is read.
        for _ in 0..3 {
            run_app_frame(&ctx, &mut app, Vec::new());
        }
        let texts = frame_texts(&ctx, &mut app);
        for present in ["Reply all", "Cc", "Bcc", "Subject"] {
            assert!(
                texts.iter().any(|text| text == present),
                "{present} is on the popup's form: {texts:?}"
            );
        }
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        assert!(
            !fills_in_color(&shapes, theme::of(&ctx).card)
                .iter()
                .any(|rect| (rect.height() - COMPOSE_BUTTON_HEIGHT).abs() < 0.5),
            "the popup keeps egui's own two buttons"
        );
    }

    /// The header's delete cell takes the one mail whose header it is on: a
    /// delete job is what the click starts — the same `Job::DeleteMessage`
    /// the row's own button and the title bar's Delete send, which is
    /// bracketed by `SyncStarted` and reaches a server the fixture does not
    /// have. That the job started is what is observable here.
    #[test]
    fn the_headers_delete_cell_takes_that_one_mail() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        app.selected_message = Some("1".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());
        while app.rx.try_recv().is_ok() {}

        click_at(&ctx, &mut app, header_action_center(0));
        assert!(
            sync_starts_within(&mut app, Duration::from_secs(5)),
            "the delete cell started a delete job"
        );
    }

    /// A mail's head is its sender: the list's own avatar at the card's
    /// top-left, the sender's name beside it, and the mail's four actions at
    /// that row's right end — all of it above the subject, which is what the
    /// row is there for. The address line the card used to draw is gone with
    /// it: the sender is the head, not a `From:` label under one.
    #[test]
    fn a_mails_card_puts_its_sender_first_with_the_actions_on_that_row() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        app.selected_message = Some("0".to_string());
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);

        // The avatar is the list's own circle, in the card's own padding —
        // the row the action cells are centred in.
        let pane = egui::Rect::from_min_max(
            egui::pos2(app.list_column.right(), 0.0),
            egui::pos2(1400.0, 900.0),
        );
        let avatars = avatars_in(&shapes, pane);
        assert_eq!(avatars.len(), 1, "the head draws one avatar: {avatars:?}");
        let (centre, diameter) = avatars[0];
        assert!((diameter - AVATAR_DIAMETER).abs() < 0.5);
        let card_top = TITLEBAR_HEIGHT + CONTENT_GUTTER;
        assert!(
            (centre.x - (pane.left() + CONTENT_GUTTER + CONTENT_PADDING.x + AVATAR_DIAMETER / 2.0))
                .abs()
                < 1.0
                && (centre.y - (card_top + CONTENT_PADDING.y + AVATAR_DIAMETER / 2.0)).abs() < 1.0,
            "the avatar starts the head row: {centre:?}"
        );

        // The sender is on the card; the `From:` line is not.
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts.iter().any(|text| text == "Ada <ada@example.com>"),
            "the sender's own line is on the card: {texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text.starts_with("From: ")),
            "the head is the sender, not a From: line: {texts:?}"
        );

        // The actions are cells of the card's own, smaller than the bar's:
        // what the pointer fills under one is `CARD_ACTION_SIZE` square,
        // where a title-bar cell fills the whole `TITLEBAR_BUTTON_SIZE`.
        let scheme = theme::of(&ctx);
        let cell = header_action_center(4);
        let filled = fill_under_the_pointer(&ctx, &mut app, 1400.0, cell, CARD_ACTION_SIZE)
            .unwrap_or_else(|| panic!("nothing filled under the reply cell at {cell:?}"));
        assert_eq!(filled.rect.center(), cell);
        assert_eq!(
            filled.corner_radius,
            egui::CornerRadius::same(TITLEBAR_BUTTON_CORNER_RADIUS),
            "the card's cells fill the way the bar's do"
        );

        // And the glyph is weak at rest, the text color under the pointer:
        // the four are on the mail's own head, so none of them shouts until
        // the pointer says which one is being read.
        let reply = egui::Rect::from_center_size(cell, egui::Vec2::splat(CARD_ACTION_SIZE));
        let rest = glyph_colors_in(&shapes, reply);
        assert!(
            rest.contains(&scheme.text_weak) && !rest.contains(&scheme.text),
            "the reply glyph is weak while the pointer is away: {rest:?}"
        );
        let hovered = glyph_colors_in(&frame_shapes_at(&ctx, &mut app, 1400.0), reply);
        assert!(
            hovered.contains(&scheme.text) && !hovered.contains(&scheme.text_weak),
            "the pointer brings the reply glyph to the text color: {hovered:?}"
        );
    }

    /// The colors a frame drew the line-drawing glyphs inside `rect` in: the
    /// paths the app's icons are, with text and fills left out.
    fn glyph_colors_in(
        shapes: &[egui::epaint::ClippedShape],
        rect: egui::Rect,
    ) -> Vec<egui::Color32> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_glyph_colors(&clipped.shape, rect, &mut out);
        }
        out
    }

    fn collect_glyph_colors(shape: &egui::Shape, rect: egui::Rect, out: &mut Vec<egui::Color32>) {
        match shape {
            egui::Shape::Path(path) if path.fill == egui::Color32::TRANSPARENT => {
                if rect.contains_rect(egui::Rect::from_points(&path.points)) {
                    if let egui::epaint::ColorMode::Solid(color) = path.stroke.color {
                        out.push(color);
                    }
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_glyph_colors(shape, rect, out);
                }
            }
            _ => {}
        }
    }

    /// The reading pane's cards, top to bottom: the content-colored fills that
    /// span the pane, gutter to gutter — the mails' cards and, while a compose
    /// is open on the spot, the answer's own under its mail.
    fn pane_cards(ctx: &egui::Context, app: &mut KimoApp) -> Vec<egui::Rect> {
        let pane_left = app.list_column.right();
        let mut cards: Vec<egui::Rect> =
            fills_in_color(&frame_shapes_at(ctx, app, 1400.0), theme::of(ctx).content)
                .into_iter()
                .filter(|rect| {
                    rect.center().x > pane_left
                        && rect.left() <= pane_left + CONTENT_GUTTER + 0.5
                        && rect.right() >= 1400.0 - CONTENT_GUTTER - 0.5
                })
                .collect();
        cards.sort_by(|a, b| a.top().total_cmp(&b.top()));
        cards
    }

    /// The reply's To row's own chevron: the last of the answer card's line
    /// drawings — the head bar's From list ends with one too, and it is drawn
    /// first. What a chevron is, and what else a card draws, is
    /// [`chevrons_in`]'s.
    fn fold_chevron(shapes: &[egui::epaint::ClippedShape], card: egui::Rect) -> Vec<egui::Pos2> {
        chevrons_in(shapes, card).pop().unwrap_or_default()
    }

    /// The chevrons a frame drew inside `area`, in the order it drew them: the
    /// answer's own two line drawings — the From list's at the end of its head
    /// bar, the To row's under it — found by their shape rather than by working
    /// out where those rows end. What else a card draws is its head's rule (a
    /// plain segment), Send (two fills) and Discard (a two-point line and two
    /// four-point ones).
    fn chevrons_in(
        shapes: &[egui::epaint::ClippedShape],
        area: egui::Rect,
    ) -> Vec<Vec<egui::Pos2>> {
        three_point_paths(shapes, area, true)
    }

    /// The filled ones: the solid triangle egui gives a combo box is a
    /// three-point path with a fill, where a chevron is a line drawing and has
    /// none at all.
    fn triangles_in(
        shapes: &[egui::epaint::ClippedShape],
        area: egui::Rect,
    ) -> Vec<Vec<egui::Pos2>> {
        three_point_paths(shapes, area, false)
    }

    /// Frames enough for a fade to be over: a modal comes in over a few of them,
    /// and a shape drawn while it is still coming in is a shape at half its
    /// colour — which is what a test that reads colours has to wait out.
    const FADE_FRAMES: usize = 16;

    /// The boxes a frame drew a line around, inside `area`, with the colour each
    /// line was drawn in: the rounded rects with a stroke on them, which is what
    /// a box drawn by hand is — a field's (`Field`), the writing area's
    /// (`compose_body`), the outline buttons' own. A fill is a rect without one,
    /// and a glyph is a path or a segment (`stroke_bounds`).
    fn stroked_boxes_in(
        shapes: &[egui::epaint::ClippedShape],
        area: egui::Rect,
    ) -> Vec<(egui::Rect, egui::Color32)> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_stroked_boxes(&clipped.shape, area, &mut out);
        }
        out
    }

    fn collect_stroked_boxes(
        shape: &egui::Shape,
        area: egui::Rect,
        out: &mut Vec<(egui::Rect, egui::Color32)>,
    ) {
        match shape {
            egui::Shape::Rect(rect) if rect.stroke.width > 0.0 => {
                if area.contains_rect(rect.rect) {
                    out.push((rect.rect, rect.stroke.color));
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_stroked_boxes(shape, area, out);
                }
            }
            _ => {}
        }
    }

    /// The three-point paths a frame drew inside `area`, as the points each was
    /// drawn through: with `unfilled`, the line drawings — the answer's
    /// chevrons — and without it the filled ones, which a three-point path can
    /// only be a triangle.
    fn three_point_paths(
        shapes: &[egui::epaint::ClippedShape],
        area: egui::Rect,
        unfilled: bool,
    ) -> Vec<Vec<egui::Pos2>> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_three_point_paths(&clipped.shape, area, unfilled, &mut out);
        }
        out
    }

    fn collect_three_point_paths(
        shape: &egui::Shape,
        area: egui::Rect,
        unfilled: bool,
        out: &mut Vec<Vec<egui::Pos2>>,
    ) {
        match shape {
            egui::Shape::Path(path)
                if path.points.len() == 3
                    && (path.fill == egui::Color32::TRANSPARENT) == unfilled =>
            {
                let bounds = egui::Rect::from_points(&path.points);
                if area.contains_rect(bounds) {
                    out.push(path.points.clone());
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_three_point_paths(shape, area, unfilled, out);
                }
            }
            _ => {}
        }
    }

    /// The rules a frame drew inside `area`: its horizontal segments wide
    /// enough to span a card. `ui.separator` is one, and a glyph is not — the
    /// trash can's lid is a two-point *path*, not a segment, and nothing else
    /// on a card runs its width.
    fn horizontal_rules(
        shapes: &[egui::epaint::ClippedShape],
        area: egui::Rect,
    ) -> Vec<egui::Rect> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_horizontal_rules(&clipped.shape, area, &mut out);
        }
        out
    }

    fn collect_horizontal_rules(shape: &egui::Shape, area: egui::Rect, out: &mut Vec<egui::Rect>) {
        match shape {
            egui::Shape::LineSegment { points, .. } => {
                let bounds = egui::Rect::from_two_pos(points[0], points[1]);
                if bounds.width() > area.width() * 0.5
                    && bounds.height() < 1.0
                    && area.contains_rect(bounds)
                {
                    out.push(bounds);
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_horizontal_rules(shape, area, out);
                }
            }
            _ => {}
        }
    }

    /// The answer a mail's header opens is one more card of the pane, right
    /// under the mail it answers: the conversation stays whole on screen and
    /// the card sits in it — here between the mail answered and the rest of
    /// the thread, a card gap from each. And it is the clicked mail's answer,
    /// not the conversation's opened mail's: the header the click was on is
    /// the mail the draft is made of.
    #[test]
    fn the_answer_sits_right_under_the_mail_it_answers() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_one_conversation(&ctx);
        app.selected_message = Some("3".to_string());
        // A frame to settle the columns; then the conversation is two cards.
        let _ = pane_cards(&ctx, &mut app);
        assert_eq!(
            pane_cards(&ctx, &mut app).len(),
            2,
            "the conversation is a card per mail"
        );

        // The oldest mail's reply cell — the conversation was opened on the
        // newest, and the answer is the oldest's own.
        click_at(&ctx, &mut app, header_action_center(4));
        let draft = app.compose.as_ref().expect("the reply opened a draft");
        assert_eq!(app.compose_spot, ComposeSpot::Pane);
        assert_eq!(
            app.compose_anchor
                .as_ref()
                .expect("the draft knows its mail")
                .uid,
            "1"
        );
        assert!(
            draft.body.contains("body of 1"),
            "the draft quotes the mail whose header was clicked: {}",
            draft.body
        );

        // The card sits between the two mails, a card gap from each.
        let cards = pane_cards(&ctx, &mut app);
        assert_eq!(
            cards.len(),
            3,
            "the answer is a card of the pane: {cards:?}"
        );
        assert!(
            (cards[1].top() - cards[0].bottom() - CONVERSATION_CARD_GAP).abs() < 0.5,
            "the answer is right under the mail it answers: {cards:?}"
        );
        assert!(
            (cards[2].top() - cards[1].bottom() - CONVERSATION_CARD_GAP).abs() < 0.5,
            "the thread goes on under the answer: {cards:?}"
        );

        // And nothing the pane held was replaced by the answer: the mail above
        // it still says its piece, and the answer's own card is under it. The
        // thread's tail is below the fold by then — bringing the answer into
        // view is what the pane spent its room on (`compose_inline_card`) — so
        // the two mails are counted as cards rather than read off one frame.
        let texts = frame_texts(&ctx, &mut app);
        for expected in ["body of 1", "Files"] {
            assert!(
                texts.iter().any(|text| text == expected),
                "{expected} is on the pane: {texts:?}"
            );
        }
    }

    /// An answer opened on the spot comes to the reader rather than waiting
    /// below the fold, and its keyboard is where the writing goes: the pane
    /// scrolls the new card into view on the frame it is first drawn — a reply
    /// cell at the top of a long mail is what opens it, and the card under that
    /// mail is otherwise off screen — and the caret is put in the body, above
    /// the quoted original, because the answer already knows who it goes to.
    #[test]
    fn an_answer_opened_on_the_spot_is_revealed_and_typed_into_at_its_body() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        // A mail long enough that the card under it opens below the pane.
        let long: String = (0..60).map(|line| format!("line {line}\n")).collect();
        app.states
            .get_mut("acct")
            .expect("the fixture account")
            .mailbox
            .folders[0]
            .messages[0]
            .body_text = long;
        app.selected_message = Some("0".to_string());
        // A frame on the mail itself: its header starts the pane, and nothing
        // has scrolled it.
        run_app_frame(&ctx, &mut app, Vec::new());
        assert_eq!(app.mail_scroll, 0.0, "the pane sits on the mail's head");

        click_at(&ctx, &mut app, header_action_center(4));
        assert_eq!(app.compose_spot, ComposeSpot::Pane);
        assert_eq!(
            app.compose.as_ref().expect("the reply opened a draft").to,
            "Ada <ada@example.com>",
            "a reply arrives addressed, which is what puts the keyboard in the body"
        );
        // The scroll is egui's own, animated (`Ui::scroll_to_rect`), so the pane
        // is given the frames it asks for.
        for _ in 0..40 {
            run_app_frame(&ctx, &mut app, Vec::new());
        }
        assert!(
            app.mail_scroll > 0.0,
            "the pane scrolled the answer up: {}",
            app.mail_scroll
        );

        // And it scrolled to the answer: the card is on screen whole — the
        // mail above gave way by no more than the card's own height — where
        // before the click the pane held only the head of a mail that ran on
        // below its floor.
        let answer = *pane_cards(&ctx, &mut app)
            .last()
            .expect("the answer is a card of the pane");
        let window = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1400.0, 900.0));
        assert!(
            window.contains_rect(answer),
            "the answer is on screen: {answer:?}"
        );

        // The keyboard is in the body: what is typed is the reply, and it goes
        // in *above* the quote rather than appended after it.
        run_app_frame(&ctx, &mut app, vec![egui::Event::Text("Hello".to_string())]);
        let draft = app.compose.as_ref().expect("the draft is still open");
        assert!(
            draft.body.starts_with("HelloOn ") && draft.body.contains("> line 0"),
            "the reply starts above the quoted original: {:?}",
            draft.body
        );
        assert_eq!(
            draft.to, "Ada <ada@example.com>",
            "the typing did not land in To"
        );
    }

    /// A forward is the one answer opened on the spot that has no recipient
    /// yet, so its keyboard starts at To — the body's opening caret is for an
    /// answer that already knows who it goes to. The flat To takes the keyboard
    /// like any field: it is the box that is gone, not the field.
    #[test]
    fn a_forward_opened_on_the_spot_starts_at_to() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());

        // Two cells in from the right: forward, between label and reply all.
        click_at(&ctx, &mut app, header_action_center(2));
        assert_eq!(app.compose_spot, ComposeSpot::Pane);
        let draft = app.compose.as_ref().expect("the forward opened a draft");
        assert_eq!(draft.kind, ComposeKind::Forward);
        assert_eq!(draft.to, "", "a forward is addressed by the sender");

        // A frame for the card's own to take the keyboard, then the typing.
        run_app_frame(&ctx, &mut app, Vec::new());
        run_app_frame(
            &ctx,
            &mut app,
            vec![egui::Event::Text("bob@example.com".to_string())],
        );
        let draft = app.compose.as_ref().expect("the draft is still open");
        assert_eq!(draft.to, "bob@example.com", "the typing landed in To");
        assert!(
            draft.body.starts_with("On "),
            "and not in the body: {:?}",
            draft.body
        );
    }

    /// The files come after the mail the answer says and not before it: the
    /// body is what the card is for and what the mail above it is answered
    /// with, and a file is something put *with* it. The popup keeps its form's
    /// order — files over body — where the row of attachments belongs to the
    /// fields above it rather than to the letter below them.
    #[test]
    fn the_files_row_waits_under_the_mail_written_on_the_spot() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());
        click_at(&ctx, &mut app, header_action_center(4));
        let _ = frame_texts(&ctx, &mut app);

        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let answer = *pane_cards(&ctx, &mut app)
            .last()
            .expect("the answer is a card of the pane");
        // The body is the one field the card draws as a box (the scheme's
        // input fill, `compose_body`); the Files label is the row's own.
        let body = *fills_in_color(&shapes, theme::of(&ctx).input)
            .iter()
            .find(|rect| answer.contains(rect.center()))
            .expect("the body is on the card");
        let files = text_boxes_of(&shapes, answer, "Files")
            .into_iter()
            .next()
            .expect("the files row is on the card");
        assert!(
            files.top() > body.bottom(),
            "the files wait under the mail: files {files:?} of body {body:?}"
        );
    }

    /// The body is the whole writing area and not the dozen lines an empty
    /// draft happens to need: the box fills `COMPOSE_BODY_HEIGHT`, the room
    /// the card gives it, so the row under it is not read across a band of
    /// the card's own surface. It is the card's whole column too — padding to
    /// padding, the width the head bar and the mail above it read at — rather
    /// than the field column the labelled rows use, so the mail being written
    /// is as wide as the card will hold. The box still stops at that height
    /// when the mail outgrows it — that is the scroll area's own `max_height`
    /// — and what it holds scrolls under the box rather than the box growing
    /// with it (`compose_body`).
    #[test]
    fn the_answers_body_fills_the_writing_area() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());
        click_at(&ctx, &mut app, header_action_center(4));
        let _ = frame_texts(&ctx, &mut app);

        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let answer = *pane_cards(&ctx, &mut app)
            .last()
            .expect("the answer is a card of the pane");
        let body = *fills_in_color(&shapes, theme::of(&ctx).input)
            .iter()
            .find(|rect| answer.contains(rect.center()))
            .expect("the body is on the card");
        assert!(
            (body.height() - COMPOSE_BODY_HEIGHT).abs() < 1.0,
            "the box is the writing area, {COMPOSE_BODY_HEIGHT} tall: {body:?}"
        );
        assert!(
            (body.width() - (answer.width() - CONTENT_PADDING.x * 2.0)).abs() < 1.0,
            "the box is the card's whole column: {body:?} of {answer:?}"
        );
    }

    /// The card ends with its own content: the buttons are the last row on it
    /// and its surface closes `CONTENT_PADDING` under them. The row is a
    /// bounded one (`ui.horizontal`, a row the height of what is in it) — a
    /// `with_layout` over the space left would center the pair in the whole
    /// of it and carry the card's surface down to the pane's floor, which is
    /// what left the empty band between the reason and the buttons.
    #[test]
    fn the_answers_card_ends_where_its_content_does() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());
        click_at(&ctx, &mut app, header_action_center(4));
        let _ = frame_texts(&ctx, &mut app);

        // The fixture account cannot send, so the reason line is there too:
        // the row above the buttons is the worst case for the card's end.
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let answer = *pane_cards(&ctx, &mut app)
            .last()
            .expect("the answer is a card of the pane");
        let scheme = theme::of(&ctx);
        let buttons: Vec<egui::Rect> = fills_in_color(&shapes, scheme.card)
            .into_iter()
            .filter(|rect| {
                (rect.height() - COMPOSE_BUTTON_HEIGHT).abs() < 0.5
                    && answer.contains(rect.center())
            })
            .collect();
        assert_eq!(buttons.len(), 2, "Send and Discard: {buttons:?}");
        let bottom = buttons.iter().fold(f32::NEG_INFINITY, |bottom, button| {
            bottom.max(button.bottom())
        });
        assert!(
            (answer.bottom() - bottom - CONTENT_PADDING.y).abs() < 0.5,
            "the card closes under the buttons: card {answer:?} of {buttons:?}"
        );
    }

    /// A field's box is Kimo's, and its line says who has the keyboard: the
    /// accent while it does, the theme's own border while it waits. The accent
    /// is the reader's to choose, so the same card is drawn in whatever colour
    /// they picked — here Teal, which neither scheme ships.
    #[test]
    fn a_focused_field_wears_the_accent_and_a_waiting_one_the_border() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.accent = Accent::parse("teal");
        // A new mail in the popup: its form is boxes, and its first frame puts
        // the keyboard in To, which is the field the ring is for. The frames
        // are what the modal's own fade needs: a shape drawn while it is
        // still coming in is a shape at half its colour.
        app.compose = Some(ComposeDraft::new(Some("acct".to_string())));
        app.compose_spot = ComposeSpot::Popup;
        for _ in 0..FADE_FRAMES {
            run_app_frame(&ctx, &mut app, Vec::new());
        }
        let scheme = theme::of(&ctx);
        assert_eq!(
            scheme.accent,
            app.accent.colors(scheme.dark).accent,
            "the chosen accent is the one the app draws with"
        );

        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let lines = stroked_boxes_in(&shapes, popup_area());
        let line_of = |field: &egui::Rect| {
            lines
                .iter()
                .find(|(rect, _)| rect == field)
                .map(|(_, color)| *color)
        };
        // Each field's box is the input fill on the row of its own label.
        let fields = fills_in_color(&shapes, scheme.input);
        let field_box = |label: &str| {
            let label = text_boxes_of(&shapes, popup_area(), label)
                .into_iter()
                .next()
                .unwrap_or_else(|| panic!("the form draws no {label} row"));
            *fields
                .iter()
                .filter(|rect| rect.center().y > label.top() && rect.center().y < label.bottom())
                .min_by(|a, b| a.left().total_cmp(&b.left()))
                .unwrap_or_else(|| panic!("nothing is drawn beside {label}"))
        };
        assert_eq!(
            line_of(&field_box("To")),
            Some(scheme.accent),
            "the field with the keyboard wears the accent"
        );
        for waiting in ["Cc", "Bcc", "Subject"] {
            assert_eq!(
                line_of(&field_box(waiting)),
                Some(scheme.border),
                "{waiting} waits in the theme's own border"
            );
        }
    }

    /// The writing area's box is the card's, not the text's: it stands on the
    /// room the card gives the body — `COMPOSE_BODY_HEIGHT` of it — with its
    /// line drawn on all four sides of that box, however long the mail is. The
    /// fixtures' answers quote one line, but an answer quotes a *mail*, and a
    /// text edit's own frame follows what it holds: a frame taller than the
    /// viewport runs its ends outside the room, where the scroll takes them —
    /// the top out of sight after a scroll down, the bottom before one.
    #[test]
    fn the_answers_body_box_is_the_writing_area_and_not_the_mail() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());
        click_at(&ctx, &mut app, header_action_center(4));
        // A mail longer than the room the card gives it: what a quoted mail
        // is, and what no fixture's own answer is.
        app.compose.as_mut().expect("the reply opened a draft").body =
            (0..60).map(|line| format!("> line {line}\n")).collect();
        for _ in 0..2 {
            run_app_frame(&ctx, &mut app, Vec::new());
        }

        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let answer = *pane_cards(&ctx, &mut app)
            .last()
            .expect("the answer is a card of the pane");
        // The writing area is the one field the card draws as a box: the
        // scheme's own input fill. It is looked for as the box the card's
        // middle sits in, so a box that grew with the mail is found too —
        // which is what the field's own frame used to be — and is measured
        // below.
        let body = *fills_in_color(&shapes, theme::of(&ctx).input)
            .iter()
            .find(|rect| rect.contains(answer.center()))
            .expect("the body is on the card");
        assert!(
            (body.height() - COMPOSE_BODY_HEIGHT).abs() < 1.0,
            "the box is the writing area, not the mail: {body:?}"
        );
        let boxes = stroked_boxes_in(&shapes, answer);
        assert!(
            boxes.iter().any(|(rect, _)| *rect == body),
            "and its line is drawn on that box: {boxes:?} of {body:?}"
        );
    }

    /// A window too short for the card is the *pane's* to scroll: the writing
    /// area keeps `COMPOSE_BODY_HEIGHT` rather than being squeezed into the room
    /// left under the To row, and the card's own surface covers all of it — the
    /// card runs past the pane's floor, and the pane's bar is what reaches the
    /// rest. The area is claimed outright (`compose_body`'s `scope_builder`), so
    /// a pane a few hundred points tall does not leave a viewport of two lines
    /// over a box `COMPOSE_BODY_HEIGHT` tall.
    #[test]
    fn the_answers_body_keeps_its_height_in_a_short_window() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.selected_message = Some("0".to_string());
        let short = egui::vec2(1400.0, 320.0);
        run_app_frame_sized(&ctx, &mut app, Vec::new(), short);

        // The reply cell, clicked at that window size: moved onto, down, up.
        let cell = header_action_center(4);
        run_app_frame_sized(&ctx, &mut app, vec![egui::Event::PointerMoved(cell)], short);
        for pressed in [true, false] {
            run_app_frame_sized(
                &ctx,
                &mut app,
                vec![egui::Event::PointerButton {
                    pos: cell,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                }],
                short,
            );
        }
        for _ in 0..3 {
            run_app_frame_sized(&ctx, &mut app, Vec::new(), short);
        }

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, short)),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| app.ui(ui));
        output.textures_delta.clear();
        let scheme = theme::of(&ctx);
        let body = *fills_in_color(&output.shapes, scheme.input)
            .iter()
            .find(|rect| rect.center().x > app.list_column.right())
            .expect("the body is on the pane");
        assert!(
            (body.height() - COMPOSE_BODY_HEIGHT).abs() < 1.0,
            "the writing area keeps its height: {body:?}"
        );
        let card = *fills_in_color(&output.shapes, scheme.content)
            .iter()
            .find(|rect| rect.contains(body.center()))
            .expect("the answer's card is under the body");
        assert!(
            card.contains_rect(body),
            "the card's surface covers the writing area: {card:?} of {body:?}"
        );
    }

    /// The reply's To row ends with the chevron that folds the Cc and Bcc
    /// lines out under it: down while they are away, up once they are under
    /// the row. An answer that needs no copies shows the one address it goes
    /// to, and a Reply all's own Cc is one click from being read — and
    /// edited — rather than only being copied in the dark.
    #[test]
    fn the_replys_to_row_folds_cc_and_bcc_out() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        // A mail with two recipients, one of them the user's own address, so
        // a Reply all has somebody else to copy.
        if let Some(account) = app.accounts.first_mut() {
            account.username = "me@example.com".to_string();
        }
        {
            let mail = &mut app
                .states
                .get_mut("acct")
                .expect("the fixture account")
                .mailbox
                .folders[0]
                .messages[0];
            mail.to = "me@example.com, Bob <bob@example.com>".to_string();
        }
        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());

        // Reply all: To the sender, Cc the other recipient — folded away.
        click_at(&ctx, &mut app, header_action_center(3));
        let draft = app.compose.as_ref().expect("the reply opened a draft");
        assert_eq!(app.compose_spot, ComposeSpot::Pane);
        assert_eq!(draft.kind, ComposeKind::ReplyAll);
        assert_eq!(draft.cc, "bob@example.com", "the copy is in the draft");
        let texts = frame_texts(&ctx, &mut app);
        for absent in ["Cc", "Bcc", "bob@example.com"] {
            assert!(
                !texts.iter().any(|text| text == absent),
                "{absent} is folded away at first: {texts:?}"
            );
        }

        // The chevron at the end of the row: a line drawing pointing down.
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let answer = *pane_cards(&ctx, &mut app)
            .last()
            .expect("the answer is a card of the pane");
        let fold = fold_chevron(&shapes, answer);
        assert_eq!(fold.len(), 3, "one chevron on the answer: {fold:?}");
        assert!(
            fold[1].y > fold[0].y && fold[1].y > fold[2].y,
            "it points down while the copies are away: {fold:?}"
        );

        // A click folds them out: the two labels, and what the draft holds.
        let chevron = egui::Rect::from_points(&fold).center();
        click_at(&ctx, &mut app, chevron);
        let texts = frame_texts(&ctx, &mut app);
        for shown in ["Cc", "Bcc", "bob@example.com"] {
            assert!(
                texts.iter().any(|text| text == shown),
                "{shown} is out under the To row: {texts:?}"
            );
        }
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let fold = fold_chevron(&shapes, answer);
        assert!(
            fold[1].y < fold[0].y && fold[1].y < fold[2].y,
            "it points up once they are under the row: {fold:?}"
        );

        // And the same click folds them away again.
        click_at(&ctx, &mut app, chevron);
        let texts = frame_texts(&ctx, &mut app);
        for absent in ["Cc", "Bcc", "bob@example.com"] {
            assert!(
                !texts.iter().any(|text| text == absent),
                "{absent} is folded away again: {texts:?}"
            );
        }
    }

    /// The answer's From list ends with the chevron its To row ends with rather
    /// than the solid triangle a stock combo box carries: the head bar is a
    /// letter's line, so the two rows of the answer that end in something to
    /// open are drawn alike — one line drawing, down while what it holds is
    /// away and up while it is out. The popup's own From keeps egui's triangle:
    /// there the card is a form of boxes, and each box wears that form's own
    /// marks.
    #[test]
    fn the_answers_from_list_ends_with_the_to_rows_chevron() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());
        // Reply: the answer is written under its mail, on the spot.
        click_at(&ctx, &mut app, header_action_center(4));
        assert_eq!(app.compose_spot, ComposeSpot::Pane);

        // The head bar is the card down to the rule under it, and the From
        // list's own chevron is the only glyph drawn in there: a line drawing,
        // pointing down while the list is away — no filled triangle standing in
        // its place on the head of a letter.
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let answer = *pane_cards(&ctx, &mut app)
            .last()
            .expect("the answer is a card of the pane");
        let rule = *horizontal_rules(&shapes, answer)
            .first()
            .expect("the head bar's rule");
        let head = egui::Rect::from_min_max(answer.min, egui::pos2(answer.max.x, rule.top()));
        let from = chevrons_in(&shapes, head);
        assert_eq!(from.len(), 1, "one chevron on the head bar: {from:?}");
        assert!(
            from[0][1].y > from[0][0].y && from[0][1].y > from[0][2].y,
            "it points down while the list is away: {from:?}"
        );
        let triangle = triangles_in(&shapes, head);
        assert!(
            triangle.is_empty(),
            "and no solid triangle stands in its place: {triangle:?}"
        );

        // The two rows end with the same drawing: the second chevron of the
        // card is the To row's, under the head's rule, cut to the same size.
        let chevrons = chevrons_in(&shapes, answer);
        assert_eq!(chevrons.len(), 2, "one on each row: {chevrons:?}");
        assert!(
            chevrons[1][0].y > rule.top(),
            "the second is the To row's, under the rule: {chevrons:?}"
        );
        let (first, second) = (
            egui::Rect::from_points(&chevrons[0]).size(),
            egui::Rect::from_points(&chevrons[1]).size(),
        );
        assert!(
            (first.x - second.x).abs() < 0.5 && (first.y - second.y).abs() < 0.5,
            "one drawing, on both rows: {chevrons:?}"
        );

        // It is drawn the way the row under it is drawn too: the scheme's
        // weak text while the pointer is away, its own text color under the
        // hand — the flat list's hover fill is what says the button answers
        // the pointer at all.
        let from_chevron = egui::Rect::from_points(&from[0]).center();
        let scheme = theme::of(&ctx);
        let rest = glyph_colors_in(&shapes, head);
        assert!(
            rest.contains(&scheme.text_weak) && !rest.contains(&scheme.text),
            "the From chevron is weak while the pointer is away: {rest:?}"
        );
        let lit = glyph_colors_in(
            &frame_shapes_with(
                &ctx,
                &mut app,
                1400.0,
                vec![egui::Event::PointerMoved(from_chevron)],
            ),
            head,
        );
        assert!(
            lit.contains(&scheme.text) && !lit.contains(&scheme.text_weak),
            "the pointer brings it to the text color, as it brings the To row's: {lit:?}"
        );

        // A click on it opens the list under the row — the combo is still the
        // combo, with the drawing changed — and the chevron turns up while the
        // list is out, the way the To row's turns over the lines it folds out.
        let label = "Test <ada>";
        let count = |ctx: &egui::Context, app: &mut KimoApp| {
            frame_texts(ctx, app)
                .into_iter()
                .filter(|text| text == label)
                .count()
        };
        assert_eq!(count(&ctx, &mut app), 1, "the list is away at first");
        click_at(&ctx, &mut app, from_chevron);
        assert_eq!(
            count(&ctx, &mut app),
            2,
            "the click opened the list under the row"
        );
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let out = chevrons_in(&shapes, head);
        assert_eq!(out.len(), 1, "still one chevron on the head bar: {out:?}");
        assert!(
            out[0][1].y < out[0][0].y && out[0][1].y < out[0][2].y,
            "it points up while the list is under the row: {out:?}"
        );

        // Another click puts it away again, and the drawing is back where it
        // started: the same two states the To row's own chevron has.
        click_at(&ctx, &mut app, from_chevron);
        assert_eq!(count(&ctx, &mut app), 1, "the click closed the list");
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let shut = chevrons_in(&shapes, head);
        assert_eq!(shut.len(), 1, "one chevron on the head bar: {shut:?}");
        assert!(
            shut[0][1].y > shut[0][0].y && shut[0][1].y > shut[0][2].y,
            "it points down again: {shut:?}"
        );

        // The popup is the same card in its other container, and there the From
        // list keeps egui's own filled triangle: it is a box among boxes.
        let mail = app.selected_mail().cloned().expect("the mail to answer");
        app.compose_about(MailAction::Reply, ComposeSpot::Popup, &mail);
        for _ in 0..3 {
            run_app_frame(&ctx, &mut app, Vec::new());
        }
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let from_label = text_boxes_of(&shapes, popup_area(), "From")
            .into_iter()
            .next()
            .expect("the popup's own From row");
        // The row the label is on, out to the card's own width — the label is
        // right-aligned in the card's label column.
        let row = egui::Rect::from_min_max(
            egui::pos2(
                from_label.right() - COMPOSE_LABEL_WIDTH,
                from_label.top() - 2.0,
            ),
            egui::pos2(
                from_label.right() - COMPOSE_LABEL_WIDTH + COMPOSE_WIDTH,
                from_label.bottom() + 2.0,
            ),
        );
        assert!(
            chevrons_in(&shapes, row).is_empty(),
            "the popup's From list is no chevron"
        );
        assert_eq!(
            triangles_in(&shapes, row).len(),
            1,
            "it keeps egui's own filled triangle"
        );
    }
    /// The on-the-spot card's foot is a letter's, not a form's: no rule over
    /// it — the mail above is what the card's end is read against — and what
    /// the card has to say (here, why Send cannot be pressed) on a row of its
    /// own, over the two buttons rather than beside them.
    #[test]
    fn the_answers_foot_has_no_rule_and_says_its_piece_over_the_buttons() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());
        click_at(&ctx, &mut app, header_action_center(4));
        let _ = frame_texts(&ctx, &mut app);

        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let answer = *pane_cards(&ctx, &mut app)
            .last()
            .expect("the answer is a card of the pane");
        let scheme = theme::of(&ctx);

        // One rule on the card, and it is the one under its head bar.
        let rules = horizontal_rules(&shapes, answer);
        assert_eq!(
            rules.len(),
            1,
            "the head's rule is the only one the answer draws: {rules:?}"
        );
        assert!(
            rules[0].center().y < answer.center().y,
            "and it is the head's, not a foot's: {rules:?} of {answer:?}"
        );

        // What the card says is on its own row, clear above the buttons.
        let reason = send_blocker(
            app.accounts.first(),
            &["ada@example.com".to_string()],
            &[],
            &[],
        )
        .expect("the fixture account has no outgoing server");
        let hint = text_boxes_of(&shapes, answer, &reason)
            .into_iter()
            .next()
            .expect("the reason is on the card");
        let buttons: Vec<egui::Rect> = fills_in_color(&shapes, scheme.card)
            .into_iter()
            .filter(|rect| {
                (rect.height() - COMPOSE_BUTTON_HEIGHT).abs() < 0.5
                    && answer.contains(rect.center())
            })
            .collect();
        assert_eq!(buttons.len(), 2, "Send and Discard: {buttons:?}");
        let buttons_top = buttons
            .iter()
            .fold(f32::INFINITY, |top, button| top.min(button.top()));
        assert!(
            hint.bottom() <= buttons_top + 0.5,
            "the reason stands over the buttons: {hint:?} of {buttons:?}"
        );
    }

    /// The card's own two open its foot's row rather than ending it: Discard
    /// first and Send beside it, against the card's left edge — the side the
    /// mail above is read from — where the popup keeps its stock pair at its
    /// form's right end. Send is the accent-filled one of the two
    /// (`compose_button`), and a settled draft is what lets it be: the fixture
    /// account is given an outgoing server so the pair is the enabled pair.
    #[test]
    fn the_answers_buttons_open_the_foot_at_the_cards_left_edge() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        if let Some(account) = app.accounts.first_mut() {
            account.username = "ada@example.com".to_string();
            account.smtp = Some(SmtpSettings {
                host: "smtp.example.com".to_string(),
                port: 587,
                tls: TlsMode::StartTls,
                login: None,
            });
        }
        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());
        click_at(&ctx, &mut app, header_action_center(4));
        let _ = frame_texts(&ctx, &mut app);

        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let answer = *pane_cards(&ctx, &mut app)
            .last()
            .expect("the answer is a card of the pane");
        let scheme = theme::of(&ctx);
        let button_in = |fill: egui::Color32| -> egui::Rect {
            let mut found = fills_in_color(&shapes, fill).into_iter().filter(|rect| {
                (rect.height() - COMPOSE_BUTTON_HEIGHT).abs() < 0.5
                    && answer.contains(rect.center())
            });
            let button = found.next().expect("a button of that fill");
            assert!(found.next().is_none(), "one button of that fill");
            button
        };
        let send = button_in(scheme.accent);
        let discard = button_in(scheme.card);

        assert!(
            discard.left() < send.left(),
            "Discard comes first: {discard:?} of {send:?}"
        );
        assert!(
            (discard.left() - (answer.left() + CONTENT_PADDING.x)).abs() < 0.5,
            "the pair opens the card's own column: {discard:?} of {answer:?}"
        );
    }

    /// A compose opened on the spot stays while its mail is on the pane; once
    /// the mail has left it — another folder opened under it, whose own first
    /// move is to drop the selection — the draft moves to the popup window:
    /// still a draft, in the container that needs no mail under it.
    #[test]
    fn an_answer_leaves_the_pane_with_its_mail() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());
        click_at(&ctx, &mut app, header_action_center(4));
        assert_eq!(app.compose_spot, ComposeSpot::Pane);

        app.selected_message = None;
        run_app_frame(&ctx, &mut app, Vec::new());
        assert_eq!(app.compose_spot, ComposeSpot::Popup);
        assert!(app.compose.is_some(), "the move kept the draft");
    }

    // ----- labels ------------------------------------------------------------

    /// The boxes of every run of text a frame drew as exactly `text`, inside
    /// `rect`, in the order they were painted. The same name can be drawn
    /// twice — the navigation column's label group, and a popup over it — and
    /// the one painted last is the one on top.
    fn text_boxes_of(
        shapes: &[egui::epaint::ClippedShape],
        rect: egui::Rect,
        text: &str,
    ) -> Vec<egui::Rect> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_text_boxes_of(&clipped.shape, rect, text, &mut out);
        }
        out
    }

    fn collect_text_boxes_of(
        shape: &egui::Shape,
        rect: egui::Rect,
        text: &str,
        out: &mut Vec<egui::Rect>,
    ) {
        match shape {
            egui::Shape::Text(run) => {
                let drawn = egui::Rect::from_min_size(run.pos, run.galley.size());
                if run.galley.text() == text && rect.contains_rect(drawn) {
                    out.push(drawn);
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_text_boxes_of(shape, rect, text, out);
                }
            }
            _ => {}
        }
    }

    /// Every image a frame drew inside `rect`, in the order it drew them — an
    /// image being a textured mesh, which is the shape epaint makes one from.
    fn image_rects_of(shapes: &[egui::epaint::ClippedShape], rect: egui::Rect) -> Vec<egui::Rect> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_image_rects(&clipped.shape, rect, &mut out);
        }
        out
    }

    fn collect_image_rects(shape: &egui::Shape, rect: egui::Rect, out: &mut Vec<egui::Rect>) {
        match shape {
            egui::Shape::Mesh(mesh) => {
                let drawn = mesh.calc_bounds();
                if rect.contains_rect(drawn) {
                    out.push(drawn);
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_image_rects(shape, rect, out);
                }
            }
            _ => {}
        }
    }

    /// The navigation column's own rect this frame: everything left of the
    /// message list.
    fn nav_column(app: &KimoApp) -> egui::Rect {
        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(app.list_column.left(), 900.0))
    }

    /// The part of the window a label popup opens over: right of the navigation
    /// column and below the title bar, which is where both cells that open one
    /// sit — and what keeps the column's own label row out of a search for a
    /// name the popup draws too.
    fn popup_area() -> egui::Rect {
        egui::Rect::from_min_max(
            egui::pos2(400.0, TITLEBAR_HEIGHT),
            egui::pos2(1400.0, 900.0),
        )
    }

    /// An app with two accounts, each holding one INBOX of `mails` mails named
    /// `<account>-<n>`, one second apart. What the label tests need: a label
    /// spans the whole mailbox — every account and folder at once — and the
    /// navigation column's label group stands under all of them.
    fn app_with_two_accounts(ctx: &egui::Context, mails: usize) -> KimoApp {
        let mut app = app_with_messages(ctx, 0);
        app.accounts = ["acct", "other"]
            .iter()
            .map(|id| Account {
                id: id.to_string(),
                name: id.to_string(),
                protocol: Protocol::Imap,
                host: "imap.example.com".to_string(),
                port: 993,
                username: "ada".to_string(),
                password: "secret".to_string(),
                tls: TlsMode::Implicit,
                smtp: None,
            })
            .collect();
        let mut states = HashMap::new();
        for account in &app.accounts {
            let messages: Vec<MailMessage> = (0..mails)
                .map(|index| {
                    let mut mail = test_message(&format!("{}-{index}", account.id), false);
                    mail.timestamp = Some(1_700_000_000 + index as i64);
                    mail
                })
                .collect();
            states.insert(
                account.id.clone(),
                AccountUiState {
                    folders: vec!["INBOX".to_string()],
                    mailbox: storage::Mailbox {
                        folders: vec![storage::Folder {
                            name: "INBOX".to_string(),
                            uidvalidity: Some(1),
                            synced: true,
                            listed: true,
                            can_load_older: false,
                            messages,
                        }],
                    },
                    ..Default::default()
                },
            );
        }
        app.states = states;
        app.selected_account = Some("acct".to_string());
        app.selected_folder = Some("INBOX".to_string());
        app
    }

    /// Put one label on a mail of the fixture, the way the app's own copy of the
    /// mail's labels has to be for anything to be derived from it: the marks are
    /// the message's own (`MailMessage::labels`), and what the view reads.
    fn label_mail(app: &mut KimoApp, account: &str, folder: &str, uid: &str, name: &str) {
        {
            let mail = app
                .states
                .get_mut(account)
                .and_then(|state| {
                    state
                        .mailbox
                        .folders
                        .iter_mut()
                        .find(|candidate| candidate.name == folder)
                })
                .and_then(|folder| folder.messages.iter_mut().find(|mail| mail.uid == uid))
                .unwrap_or_else(|| panic!("no mail {uid} in {account}/{folder}"));
            mail.labels.insert(name.to_string());
        }
        if !app.labels.iter().any(|label| label == name) {
            app.labels.push(name.to_string());
            app.labels.sort_by_key(|name| name.to_lowercase());
        }
        app.labels_rev = app.labels_rev.wrapping_add(1);
    }

    /// Click one label's row in the navigation column: the whole row is the
    /// target, and its name is what it draws.
    fn click_label(ctx: &egui::Context, app: &mut KimoApp, name: &str) {
        let shapes = frame_shapes_at(ctx, app, 1400.0);
        let row = text_boxes_of(&shapes, nav_column(app), name)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("the column draws no row for {name}"));
        click_at(ctx, app, row.center());
    }

    /// Where the bar's filter cell sits: the search box's own group, a cell's gap
    /// to its right (`titlebar_anchors`). Runs a frame, so the columns the plan
    /// is measured against are the ones the bar itself read.
    fn titlebar_filter_rect(ctx: &egui::Context, app: &mut KimoApp, width: f32) -> egui::Rect {
        frame_shapes_at(ctx, app, width);
        let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, TITLEBAR_HEIGHT));
        let rects = titlebar_rects(bar, 3);
        let pane_left = if app.pane_popup {
            0.0
        } else {
            app.list_column.right()
        };
        titlebar_anchors(
            bar,
            first_group_end(),
            app.list_column.left(),
            pane_left,
            rects.sync.left(),
        )
        .filter
        .expect("the bar draws a filter cell here")
    }

    /// Where the bar's sort cell sits: the filter's other half, a cell's gap to
    /// its right in the same group (`titlebar_anchors`). Runs a frame, like
    /// `titlebar_filter_rect`.
    fn titlebar_sort_rect(ctx: &egui::Context, app: &mut KimoApp, width: f32) -> egui::Rect {
        frame_shapes_at(ctx, app, width);
        let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, TITLEBAR_HEIGHT));
        let rects = titlebar_rects(bar, 3);
        let pane_left = if app.pane_popup {
            0.0
        } else {
            app.list_column.right()
        };
        titlebar_anchors(
            bar,
            first_group_end(),
            app.list_column.left(),
            pane_left,
            rects.sync.left(),
        )
        .sort
        .expect("the bar draws a sort cell here")
    }

    /// Where one of the title bar's mail action cells sits: the bar's own
    /// arithmetic, from the search box the run is laid out after onwards
    /// (`titlebar_rects`, `titlebar_anchors`).
    fn titlebar_action_center(
        ctx: &egui::Context,
        app: &mut KimoApp,
        action: MailAction,
        width: f32,
    ) -> egui::Pos2 {
        let actions = mail_action_rect(ctx, app, width)
            .unwrap_or_else(|| panic!("the bar drew no mail actions at {width} pt"));
        let index = MailAction::ALL
            .iter()
            .position(|candidate| *candidate == action)
            .expect("one of the seven");
        egui::pos2(
            actions.left()
                + index as f32 * (TITLEBAR_BUTTON_SIZE + TITLEBAR_BUTTON_GAP)
                + TITLEBAR_BUTTON_SIZE / 2.0,
            TITLEBAR_HEIGHT / 2.0,
        )
    }

    /// Where the first group ends: the nav toggle and the pen, a cell's gap
    /// between them and the bar's own margin before the first. The one anchor
    /// nothing else may cross.
    fn first_group_end() -> f32 {
        TITLEBAR_LEFT_INSET + TITLEBAR_BUTTON_GAP + 2.0 * TITLEBAR_BUTTON_SIZE
    }

    /// The rect the bar gives its mail actions this frame, from the columns the
    /// app has just laid out — `None` when the pane is too narrow to hold
    /// them beside the window controls. Runs a frame, so the app's column rects
    /// are the ones the bar itself read.
    fn mail_action_rect(ctx: &egui::Context, app: &mut KimoApp, width: f32) -> Option<egui::Rect> {
        frame_shapes_at(ctx, app, width);
        let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, TITLEBAR_HEIGHT));
        let rects = titlebar_rects(bar, 3);
        let pane_left = if app.pane_popup {
            0.0
        } else {
            app.list_column.right()
        };
        titlebar_anchors(
            bar,
            first_group_end(),
            app.list_column.left(),
            pane_left,
            rects.sync.left(),
        )
        .actions
    }

    /// The labels are a group of their own, and the navigation column's last
    /// one: under every account and its folders, headed by the word alone — a
    /// label is not an account, so there is nothing to collapse — with the mail
    /// each holds counted beside it.
    #[test]
    fn the_labels_are_the_navigation_columns_last_group() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_two_accounts(&ctx, 2);
        label_mail(&mut app, "acct", "INBOX", "acct-0", "Work");
        label_mail(&mut app, "acct", "INBOX", "acct-1", "Work");
        label_mail(&mut app, "other", "INBOX", "other-0", "Travel");

        // What the group says, before the frame draws it: both labels, in name
        // order, each with the amount of mail under it.
        app.refresh_label_view();
        let rows = &app.label_view.as_ref().expect("derived just above").rows;
        assert_eq!(
            rows.iter()
                .map(|row| (row.name.as_str(), row.count, row.selected))
                .collect::<Vec<_>>(),
            [("Travel", 1, false), ("Work", 2, false)]
        );

        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let nav = nav_column(&app);
        let heading = text_boxes_of(&shapes, nav, "Labels")
            .into_iter()
            .next()
            .expect("the group is headed");
        let work = text_boxes_of(&shapes, nav, "Work")
            .into_iter()
            .next()
            .expect("the row is drawn");
        let travel = text_boxes_of(&shapes, nav, "Travel")
            .into_iter()
            .next()
            .expect("the second row too");
        // The heading, then the rows in name order: Travel before Work.
        assert!(
            heading.top() < travel.top() && travel.bottom() <= work.top(),
            "the group draws its heading and then its rows: {heading:?} {travel:?} {work:?}"
        );
        // Under every account: the folders of both are above the heading, and
        // an account's name with them.
        for folder in ["INBOX", "acct", "other"] {
            for row in text_boxes_of(&shapes, nav, folder) {
                assert!(
                    row.bottom() <= heading.top(),
                    "{folder} is drawn under the label group: {row:?} against {heading:?}"
                );
            }
        }
    }

    /// A click on a label is a filter over the whole mailbox: the list becomes
    /// the mail carrying it — the mail of every folder and account at once, each
    /// row saying where it lives — and a click on the same label takes the
    /// filter back out.
    #[test]
    fn clicking_a_label_narrows_the_list_to_its_mail() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_two_accounts(&ctx, 2);
        // The two accounts' mail, one labelled each: the folder's own list is
        // never what the filter shows.
        label_mail(&mut app, "acct", "INBOX", "acct-0", "Work");
        label_mail(&mut app, "other", "INBOX", "other-1", "Work");

        click_label(&ctx, &mut app, "Work");
        assert_eq!(app.label_filter, ["Work".to_string()]);

        let texts = frame_texts(&ctx, &mut app);
        let drawn = |text: &str| texts.iter().any(|run| run == text);
        assert!(drawn("Subject acct-0") && drawn("Subject other-1"));
        assert!(
            !drawn("Subject acct-1") && !drawn("Subject other-0"),
            "the list is the label's mail, not the folder's: {texts:?}"
        );
        // The rows are the search rows' shape: where a mail lives, because the
        // filter spans the accounts.
        assert!(drawn("acct · INBOX") && drawn("other · INBOX"), "{texts:?}");

        // The same click again: the filter is off, and the column is the
        // folder's list.
        click_label(&ctx, &mut app, "Work");
        assert!(app.label_filter.is_empty());
        let texts = frame_texts(&ctx, &mut app);
        assert!(texts.iter().any(|run| run == "Subject acct-1"));
        assert!(!texts.iter().any(|run| run == "Subject other-1"));
    }

    /// Two labels narrow the list rather than widening it: the filter is an AND,
    /// so the list is the mail that carries both — and taking one back out
    /// leaves the other's mail.
    #[test]
    fn two_labels_filter_with_an_and_and_a_click_takes_one_back() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_two_accounts(&ctx, 2);
        label_mail(&mut app, "acct", "INBOX", "acct-0", "Work");
        label_mail(&mut app, "acct", "INBOX", "acct-1", "Work");
        label_mail(&mut app, "acct", "INBOX", "acct-1", "Travel");
        label_mail(&mut app, "other", "INBOX", "other-0", "Travel");

        click_label(&ctx, &mut app, "Work");
        let texts = frame_texts(&ctx, &mut app);
        let drawn = |text: &str| texts.iter().any(|run| run == text);
        assert!(drawn("Subject acct-0") && drawn("Subject acct-1"));
        assert!(!drawn("Subject other-0"));

        click_label(&ctx, &mut app, "Travel");
        assert_eq!(app.label_filter, ["Travel".to_string(), "Work".to_string()]);
        let texts = frame_texts(&ctx, &mut app);
        let drawn = |text: &str| texts.iter().any(|run| run == text);
        assert!(
            drawn("Subject acct-1"),
            "the one mail carrying both: {texts:?}"
        );
        assert!(
            !drawn("Subject acct-0") && !drawn("Subject other-0"),
            "an AND narrows: {texts:?}"
        );

        // One out again: the other's mail comes back.
        click_label(&ctx, &mut app, "Work");
        assert_eq!(app.label_filter, ["Travel".to_string()]);
        let texts = frame_texts(&ctx, &mut app);
        let drawn = |text: &str| texts.iter().any(|run| run == text);
        assert!(drawn("Subject acct-1") && drawn("Subject other-0"));
        assert!(!drawn("Subject acct-0"));
    }

    /// Developer tool (there is no compositor at hand in a test): draw a few
    /// frames — the labelled list and card, the label filter and popup, the
    /// flagged list, the list filter's menu and the list it leaves, the sort
    /// menu, the bar with and without a mail open in the pane, and
    /// Preferences → General — and write them out as PNGs, so the layout can be
    /// looked at. `KIMO_UI_DUMP` picks the directory (default `target`) and
    /// `KIMO_UI_WIDTH` the window's width (default 1400).
    ///
    ///     cargo test -- --ignored --nocapture dump_ui
    #[test]
    #[ignore = "developer tool: writes PNGs of a few frames and asserts nothing"]
    fn dump_ui() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_two_accounts(&ctx, 3);
        label_mail(&mut app, "acct", "INBOX", "acct-0", "Work");
        label_mail(&mut app, "acct", "INBOX", "acct-1", "Work");
        label_mail(&mut app, "acct", "INBOX", "acct-1", "Travel");
        label_mail(&mut app, "other", "INBOX", "other-0", "Travel");
        app.selected_message = Some("acct-1".to_string());
        // The first frame the app draws text on is the one that uploads the
        // font atlas, and every frame after it only patches what is there: the
        // rasterizer has to be in on that frame, so the textures outlive it.
        let mut textures = HashMap::new();
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
        let dir = std::path::PathBuf::from(
            std::env::var("KIMO_UI_DUMP").unwrap_or_else(|_| "target".to_string()),
        );
        let shot = |app: &mut KimoApp, name: &str, textures: &mut HashMap<_, _>| {
            let path = dir.join(name);
            render_frame_png(&ctx, app, &path, textures);
            println!("wrote {}", path.display());
        };
        shot(&mut app, "ui-labels-card.png", &mut textures);
        // The label row in the column, found where the frame drew it. A window
        // narrow enough for the navigation to have collapsed has no column to
        // find it in: the label states then simply are not part of the dump,
        // which is what looking at a narrow *bar* wants anyway.
        let shapes = collect_frame(&ctx, &mut app, &mut textures, Vec::new());
        let row = text_boxes_of(&shapes, nav_column(&app), "Work")
            .into_iter()
            .next();
        if let Some(row) = row {
            dump_click(&ctx, &mut app, &mut textures, row.center());
            shot(&mut app, "ui-labels-filter.png", &mut textures);
            dump_click(&ctx, &mut app, &mut textures, header_action_center(1));
            shot(&mut app, "ui-labels-popup.png", &mut textures);
        }
        // And Preferences → General, where the sync settings live. The label
        // popup is dismissed first — a menu left open over the columns would be
        // half the picture — and the modal itself is an `Area`, which lays
        // itself out on the frame it is opened on and draws on the next: one
        // frame first, or the picture would be of a window with no modal in it.
        egui::Popup::close_all(&ctx);
        // And the flag: the marked row in the list — warning fill, flag glyph,
        // and a paperclip beside it to show the two marks sharing the end of a
        // title line — with a marked mail open in the pane, whose card is the
        // plain surface it always was.
        app.set_flag_on_mail(&MailKey::new("acct", "INBOX", "acct-0"), true);
        app.set_flag_on_mail(&MailKey::new("acct", "INBOX", "acct-1"), true);
        {
            let state = app.states.get_mut("acct").expect("the fixture account");
            state.mailbox.folders[0].messages[1].attachments = vec![Attachment {
                name: "report.pdf".to_string(),
                content_type: "application/pdf".to_string(),
                size: 4096,
            }];
            state.folders_changed();
        }
        for _ in 0..16 {
            collect_frame(&ctx, &mut app, &mut textures, Vec::new());
        }
        shot(&mut app, "ui-flag.png", &mut textures);

        // And the row's own two buttons under the pointer: the flag cell just
        // inside the delete one — wearing the mark's colour on the mail that is
        // flagged — and then the flag cell itself under the pointer, filled with
        // the tint a flagged row is filled with.
        let row = checkbox_center(&app, 1, false);
        for _ in 0..2 {
            collect_frame(
                &ctx,
                &mut app,
                &mut textures,
                vec![egui::Event::PointerMoved(egui::pos2(320.0, row.y))],
            );
        }
        shot(&mut app, "ui-row-buttons.png", &mut textures);
        let flag_cell = row_flag_center(&app, 1);
        for _ in 0..2 {
            collect_frame(
                &ctx,
                &mut app,
                &mut textures,
                vec![egui::Event::PointerMoved(flag_cell)],
            );
        }
        shot(&mut app, "ui-row-flag-hover.png", &mut textures);

        // And the filter: the checklist over the list it narrows — every box
        // clear but "All" — then two of them ticked, then the list itself with
        // the menu out of the way and the cell in the accent that says a view is
        // on.
        let filter_cell = titlebar_filter_rect(&ctx, &mut app, 1400.0).center();
        dump_click(&ctx, &mut app, &mut textures, filter_cell);
        for _ in 0..2 {
            collect_frame(&ctx, &mut app, &mut textures, Vec::new());
        }
        shot(&mut app, "ui-filter-menu.png", &mut textures);
        // The boxes are independent, and a tick leaves the menu open: the
        // picture is of the AND before it is applied to the list.
        let tick = |app: &mut KimoApp, textures: &mut HashMap<_, _>, label: &str| {
            let shapes = collect_frame(&ctx, app, textures, Vec::new());
            let area = egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(dump_width(), DUMP_HEIGHT as f32),
            );
            let row = text_boxes_of(&shapes, area, label)
                .into_iter()
                .next()
                .unwrap_or_else(|| panic!("the menu draws no {label} row"));
            dump_click(&ctx, app, textures, row.center());
        };
        tick(&mut app, &mut textures, "Unread");
        tick(&mut app, &mut textures, "Has files");
        for _ in 0..2 {
            collect_frame(&ctx, &mut app, &mut textures, Vec::new());
        }
        shot(&mut app, "ui-filter-and.png", &mut textures);
        // The menu goes before the picture of the list itself: in the app it
        // closes on a click outside, and what is being looked at here is what
        // the list became — one view, the folder's own.
        egui::Popup::close_all(&ctx);
        app.set_list_filters(ListFilters(vec![ListFilter::Flagged]));
        for _ in 0..2 {
            collect_frame(&ctx, &mut app, &mut textures, Vec::new());
        }
        shot(&mut app, "ui-filter-flagged.png", &mut textures);
        app.set_list_filters(ListFilters::default());

        // And an answer written on the spot: the card under its own mail, whose
        // head bar is the account's avatar and the From list — no heading, and no
        // Cc, Bcc or Subject. The account is given an outgoing server and an
        // address to send as first, so the shot shows the footer a working
        // account has: the accent-filled Send with the plane in its own ink,
        // rather than the muted pair a blocked draft draws.
        {
            if let Some(account) = app.accounts.first_mut() {
                account.username = "ada@example.com".to_string();
                account.smtp = Some(SmtpSettings {
                    host: "smtp.example.com".to_string(),
                    port: 587,
                    tls: TlsMode::StartTls,
                    login: None,
                });
            }
            let mail = app.selected_mail().cloned().or_else(|| {
                app.states.get("acct").and_then(|state| {
                    state
                        .mailbox
                        .folders
                        .first()
                        .and_then(|folder| folder.messages.first().cloned())
                })
            });
            if let Some(mail) = mail {
                app.compose_about(MailAction::Reply, ComposeSpot::Pane, &mail);
                for _ in 0..4 {
                    collect_frame(&ctx, &mut app, &mut textures, Vec::new());
                }
                shot(&mut app, "ui-reply-on-the-spot.png", &mut textures);
                app.compose = None;
            }
        }

        // And the compose card with a file on it: the row the attachment takes,
        // its size, the Remove, and the box that is the other way in.
        {
            app.compose = Some(ComposeDraft::new(Some("acct".to_string())));
            if let Some(draft) = app.compose.as_mut() {
                draft.to = "bob@example.com".to_string();
                draft.subject = "The report".to_string();
                draft.body = "Here it is.\n".to_string();
                let path = std::env::temp_dir().join("kimo-dump-report.pdf");
                let _ = std::fs::write(&path, vec![b'%'; 4096]);
                let _ = attach_file(draft, path);
            }
            for _ in 0..4 {
                collect_frame(&ctx, &mut app, &mut textures, Vec::new());
            }
            shot(&mut app, "ui-compose-files.png", &mut textures);
            app.compose = None;
        }

        // And the sort cell's menu: the order group above the by group, each a
        // choice of one and ticked at the end of the row in force.
        let sort_cell = titlebar_sort_rect(&ctx, &mut app, 1400.0).center();
        dump_click(&ctx, &mut app, &mut textures, sort_cell);
        for _ in 0..2 {
            collect_frame(&ctx, &mut app, &mut textures, Vec::new());
        }
        shot(&mut app, "ui-sort-menu.png", &mut textures);
        egui::Popup::close_all(&ctx);

        // The bar with nothing open in the pane first: the pane's run is not
        // there at all rather than there and dimmed, and the chrome it would
        // have used is the drag surface. The frames are what its fade needs.
        app.selected_message = None;
        for _ in 0..16 {
            collect_frame(&ctx, &mut app, &mut textures, Vec::new());
        }
        shot(&mut app, "ui-bar-no-mail.png", &mut textures);
        app.selected_message = Some("acct-1".to_string());

        app.show_preferences = true;
        app.prefs_tab = PrefsTab::General;
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
        shot(&mut app, "ui-general.png", &mut textures);
        // And Preferences → Appearance, where the theme's radios, the accent's
        // chips and the palette they resolve to are: the whole page at once,
        // which is the one place a colour can be chosen.
        app.prefs_tab = PrefsTab::Appearance;
        for _ in 0..2 {
            collect_frame(&ctx, &mut app, &mut textures, Vec::new());
        }
        shot(&mut app, "ui-appearance.png", &mut textures);
        app.show_preferences = false;

        // And the Blitz page's paper: the sender's page on white, and on the dark
        // surface after the contrast cell on the mail's head. The page is a test
        // one — a real render is a worker's job, not a dump's — and its tile is
        // transparent, the way a mail that paints no background of its own
        // rasterizes, so what these two shots show is the paper itself.
        app.html_renderer = HtmlRenderer::Blitz;
        {
            let state = app.states.get_mut("acct").expect("the fixture account");
            state.mailbox.folders[0].messages[1].body_html = Some("<p>The mail.</p>".to_string());
        }
        app.blitz_pages.clear();
        app.blitz_order.clear();
        app.pending_blitz_key = None;
        for _ in 0..2 {
            collect_frame(&ctx, &mut app, &mut textures, Vec::new());
        }
        if let Some((key, _)) = app.pending_blitz_key.clone() {
            let (page, _) =
                ready_blitz_page(&ctx, egui::vec2(900.0, 260.0), egui::Color32::TRANSPARENT);
            app.blitz_pages.insert(key, page);
        }
        for _ in 0..2 {
            collect_frame(&ctx, &mut app, &mut textures, Vec::new());
        }
        shot(&mut app, "ui-blitz-paper.png", &mut textures);
        dump_click(&ctx, &mut app, &mut textures, header_action_center(5));
        for _ in 0..2 {
            collect_frame(&ctx, &mut app, &mut textures, Vec::new());
        }
        shot(&mut app, "ui-blitz-paper-dark.png", &mut textures);
        app.html_renderer = HtmlRenderer::Simple;
    }

    /// The dump canvas: the height every picture is drawn at, and the width the
    /// window is asked for.
    const DUMP_HEIGHT: usize = 900;

    /// The window the dump tool draws at: 1400 by 900 unless `KIMO_UI_WIDTH`
    /// says otherwise, which is how the narrow layouts — the capped pane, the
    /// popup one — are looked at.
    fn dump_width() -> f32 {
        std::env::var("KIMO_UI_WIDTH")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1400.0)
    }

    /// One headless frame of the app, rasterized straight from egui's own
    /// triangles: a frame is a mesh per texture with premultiplied vertex
    /// colors, so what a real backend would draw is a barycentric fill each.
    /// The font atlas (and any other texture the frame uploaded) is sampled for
    /// it; there is no anti-aliasing to do — egui has already feathered every
    /// edge into the vertices.
    fn render_frame_png(
        ctx: &egui::Context,
        app: &mut KimoApp,
        path: &std::path::Path,
        textures: &mut HashMap<egui::TextureId, (usize, usize, Vec<egui::Color32>)>,
    ) {
        /// The dump canvas height.
        const DUMP_HEIGHT: usize = 900;
        const HEIGHT: usize = DUMP_HEIGHT;
        let width = dump_width() as usize;

        let shapes = collect_frame(ctx, app, textures, Vec::new());
        let ppp = ctx.pixels_per_point();
        let primitives = ctx.tessellate(shapes, ppp);
        let mut canvas = vec![egui::Color32::TRANSPARENT; width * HEIGHT];
        for primitive in &primitives {
            let egui::epaint::Primitive::Mesh(mesh) = &primitive.primitive else {
                continue;
            };
            let Some((tw, th, texels)) = textures.get(&mesh.texture_id) else {
                continue;
            };
            for triangle in mesh.indices.as_chunks::<3>().0 {
                let v = [
                    mesh.vertices[triangle[0] as usize],
                    mesh.vertices[triangle[1] as usize],
                    mesh.vertices[triangle[2] as usize],
                ];
                let xs = v.map(|vertex| vertex.pos.x * ppp);
                let ys = v.map(|vertex| vertex.pos.y * ppp);
                let (x0, x1) = (
                    xs.iter()
                        .cloned()
                        .fold(f32::INFINITY, f32::min)
                        .floor()
                        .max(0.0) as usize,
                    xs.iter()
                        .cloned()
                        .fold(f32::NEG_INFINITY, f32::max)
                        .ceil()
                        .min(width as f32) as usize,
                );
                let (y0, y1) = (
                    ys.iter()
                        .cloned()
                        .fold(f32::INFINITY, f32::min)
                        .floor()
                        .max(0.0) as usize,
                    ys.iter()
                        .cloned()
                        .fold(f32::NEG_INFINITY, f32::max)
                        .ceil()
                        .min(HEIGHT as f32) as usize,
                );
                for y in y0..y1 {
                    for x in x0..x1 {
                        let point = egui::pos2((x as f32 + 0.5) / ppp, (y as f32 + 0.5) / ppp);
                        if !primitive.clip_rect.contains(point) {
                            continue;
                        }
                        let Some(weights) = barycentric(v.map(|vertex| vertex.pos), point) else {
                            continue;
                        };
                        if weights.iter().any(|weight| *weight < 0.0) {
                            continue;
                        }
                        let uv = egui::pos2(
                            v[0].uv.x * weights[0]
                                + v[1].uv.x * weights[1]
                                + v[2].uv.x * weights[2],
                            v[0].uv.y * weights[0]
                                + v[1].uv.y * weights[1]
                                + v[2].uv.y * weights[2],
                        );
                        let tx = ((uv.x * *tw as f32) as usize).min(tw.saturating_sub(1));
                        let ty = ((uv.y * *th as f32) as usize).min(th.saturating_sub(1));
                        let texel = texels[ty * *tw + tx];
                        let source = v[0].color;
                        let color = egui::Color32::from_rgba_premultiplied(
                            (source.r() as u32 * texel.r() as u32 / 255) as u8,
                            (source.g() as u32 * texel.g() as u32 / 255) as u8,
                            (source.b() as u32 * texel.b() as u32 / 255) as u8,
                            (source.a() as u32 * texel.a() as u32 / 255) as u8,
                        );
                        let dst = canvas[y * width + x];
                        let alpha = color.a() as u32;
                        canvas[y * width + x] = egui::Color32::from_rgba_premultiplied(
                            (color.r() as u32 + dst.r() as u32 * (255 - alpha) / 255) as u8,
                            (color.g() as u32 + dst.g() as u32 * (255 - alpha) / 255) as u8,
                            (color.b() as u32 + dst.b() as u32 * (255 - alpha) / 255) as u8,
                            (alpha + dst.a() as u32 * (255 - alpha) / 255) as u8,
                        );
                    }
                }
            }
        }
        let bytes: Vec<u8> = canvas.iter().flat_map(|pixel| pixel.to_array()).collect();
        let image = image::RgbaImage::from_raw(width as u32, HEIGHT as u32, bytes)
            .expect("the canvas is the size it was allocated");
        image.save(path).expect("the dump directory is writable");
    }

    /// Run one frame of the app and keep the textures it brought: the font
    /// atlas arrives on the frame that first draws a glyph and every frame after
    /// it patches what is there, so a rasterizer that misses one frame draws
    /// that frame's glyphs as nothing. Every frame a dump drives — the clicks
    /// included — has to come through here.
    fn collect_frame(
        ctx: &egui::Context,
        app: &mut KimoApp,
        textures: &mut HashMap<egui::TextureId, (usize, usize, Vec<egui::Color32>)>,
        events: Vec<egui::Event>,
    ) -> Vec<egui::epaint::ClippedShape> {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(dump_width(), DUMP_HEIGHT as f32),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| app.ui(ui));
        apply_textures(&output.textures_delta, textures);
        output.textures_delta.clear();
        output.shapes
    }

    /// A click, frame by frame, through `collect_frame` — the pointer moved onto
    /// the target and then the button down and up.
    fn dump_click(
        ctx: &egui::Context,
        app: &mut KimoApp,
        textures: &mut HashMap<egui::TextureId, (usize, usize, Vec<egui::Color32>)>,
        pos: egui::Pos2,
    ) {
        collect_frame(ctx, app, textures, vec![egui::Event::PointerMoved(pos)]);
        for pressed in [true, false] {
            collect_frame(
                ctx,
                app,
                textures,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                }],
            );
        }
    }

    /// Keep the textures a frame uploaded or patched: the font atlas whole the
    /// first time, in patches as it grows, and any picture the mail brought.
    fn apply_textures(
        delta: &egui::TexturesDelta,
        textures: &mut HashMap<egui::TextureId, (usize, usize, Vec<egui::Color32>)>,
    ) {
        for (id, deltas) in &delta.set {
            for delta in deltas {
                let egui::ImageData::Color(image) = &delta.image;
                let (width, height) = (image.size[0], image.size[1]);
                let Some([x0, y0]) = delta.pos else {
                    // A whole texture, and the font atlas is sent whole again
                    // whenever it grows: it replaces what was there, or the
                    // glyphs past the old edge would be missing.
                    textures.insert(*id, (width, height, image.pixels.clone()));
                    continue;
                };
                let Some(target) = textures.get_mut(id) else {
                    continue;
                };
                for y in 0..height {
                    for x in 0..width {
                        let (tx, ty) = (x0 + x, y0 + y);
                        if tx < target.0 && ty < target.1 {
                            target.2[ty * target.0 + tx] = image.pixels[y * width + x];
                        }
                    }
                }
            }
        }
    }

    /// The three barycentric weights of a point in a triangle, or `None` when
    /// the triangle has no area.
    fn barycentric(triangle: [egui::Pos2; 3], point: egui::Pos2) -> Option<[f32; 3]> {
        let [a, b, c] = triangle;
        let area = (b.x - a.x) * (c.y - a.y) - (c.x - a.x) * (b.y - a.y);
        if area.abs() < f32::EPSILON {
            return None;
        }
        let first = ((b.x - point.x) * (c.y - point.y) - (c.x - point.x) * (b.y - point.y)) / area;
        let second = ((c.x - point.x) * (a.y - point.y) - (a.x - point.x) * (c.y - point.y)) / area;
        Some([first, second, 1.0 - first - second])
    }

    /// A query and a label filter are two answers to what the middle column
    /// shows, and the label is the one that was just asked for: looking at a
    /// label gives an active search up rather than stacking the two into a list
    /// that is neither.
    #[test]
    fn a_label_filter_gives_up_an_active_search() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_two_accounts(&ctx, 2);
        label_mail(&mut app, "acct", "INBOX", "acct-0", "Work");
        app.search.text = "acct".to_string();
        assert!(app.search.is_active());

        click_label(&ctx, &mut app, "Work");
        assert_eq!(app.label_filter, ["Work".to_string()]);
        assert!(!app.search.is_active() && app.search.text.is_empty());
        assert!(
            app.search_applied
                .as_ref()
                .is_some_and(|query| !query.is_active()),
            "the applied query is the emptied one, so nothing is searched for it"
        );
    }

    /// The label cell on a mail's own head opens the popup about that mail, the
    /// way its reply cell opens the compose: the labels it carries are ticked,
    /// a click in the list puts one on it or takes it off, and the card says so
    /// afterwards.
    #[test]
    fn the_label_cell_labels_the_mail_it_is_on() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 2);
        app.labels = vec!["Travel".to_string(), "Work".to_string()];
        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());

        click_at(&ctx, &mut app, header_action_center(1));
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts.iter().any(|run| run == "New label…"),
            "the popup is the label list and a box: {texts:?}"
        );
        // The checklist is the app's own labels, over the columns rather than in
        // them.
        let boxes = |text: &str| text_boxes_of(&shapes, popup_area(), text);
        assert_eq!(boxes("Work").len(), 1, "the popup lists the label once");
        assert_eq!(boxes("Travel").len(), 1);

        // A click on a label in the popup puts it on this mail.
        let work = boxes("Work")[0].center();
        click_at(&ctx, &mut app, work);
        let key = MailKey::new("acct", "INBOX", "0");
        assert_eq!(app.mail_labels(&key), ["Work".to_string()]);
        // The card says it: the mail carries a chip of its own now.
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts.iter().any(|run| run == "Work"),
            "the card shows the label: {texts:?}"
        );

        // And a click on it again takes it off — the popup stayed open, and the
        // mail's chip is a different thing at another place: the popup is what
        // is drawn over the columns.
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let work = text_boxes_of(&shapes, popup_area(), "Work")
            .pop()
            .expect("the popup is still open");
        click_at(&ctx, &mut app, work.center());
        assert!(app.mail_labels(&key).is_empty());
    }

    /// A name the app does not have is made a label by being put on a mail: the
    /// popup's box is the only place one comes into being, and the navigation
    /// column's group has it from that moment on.
    #[test]
    fn a_new_label_is_made_for_the_mail_in_the_popup() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 2);
        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());
        assert!(app.labels.is_empty());

        // The label cell on the mail's head, and the popup it opens: with no
        // labels at all it says so, and offers the box.
        click_at(&ctx, &mut app, header_action_center(1));
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts
                .iter()
                .any(|run| run == "No labels yet — name one below."),
            "{texts:?}"
        );

        // Type a name in the box and press the button.
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let field = text_boxes_of(&shapes, popup_area(), "New label…")
            .pop()
            .expect("the box is in the popup");
        click_at(&ctx, &mut app, field.center());
        run_app_frame(
            &ctx,
            &mut app,
            vec![egui::Event::Text("Receipts".to_string())],
        );
        assert_eq!(app.label_draft, "Receipts", "the box holds what was typed");

        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let create = text_boxes_of(&shapes, popup_area(), "Create")
            .pop()
            .expect("the button is in the popup");
        click_at(&ctx, &mut app, create.center());

        assert_eq!(app.labels, ["Receipts".to_string()], "the label was made");
        let key = MailKey::new("acct", "INBOX", "0");
        assert_eq!(app.mail_labels(&key), ["Receipts".to_string()]);
        assert!(
            app.label_draft.is_empty(),
            "the box is emptied by the click"
        );
        // And the column draws it from here on.
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        assert!(
            !text_boxes_of(&shapes, nav_column(&app), "Receipts").is_empty(),
            "the group has the new label"
        );
    }

    /// The filter cell sits in the search box's own group and its menu is the
    /// views of the list as checkboxes — an AND between them, so ticking one
    /// never takes another off and unticking one widens the list again — with
    /// the "All" row as the way out of every view at once.
    #[test]
    fn the_filter_cell_narrows_the_list() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        // Mail "0" is unread, flagged and carries a file; "1" is read; "2" is
        // plain unread. The three marks between them are what the views are
        // about, and the subjects are what the rows say.
        {
            let state = app.states.get_mut("acct").expect("the fixture account");
            let mails = &mut state.mailbox.folders[0].messages;
            mails[1].seen = true;
            mails[0].flagged = true;
            mails[0].attachments = vec![Attachment {
                name: "report.pdf".to_string(),
                content_type: "application/pdf".to_string(),
                size: 1024,
            }];
            state.folders_changed();
        }
        let drawn =
            |texts: &[String], uid: &str| texts.iter().any(|run| run == &format!("Subject {uid}"));

        let texts = frame_texts(&ctx, &mut app);
        assert!(
            drawn(&texts, "0") && drawn(&texts, "1") && drawn(&texts, "2"),
            "every mail is in the list while no filter is on: {texts:?}"
        );

        // The cell opens the menu, which is the views as rows under their "All"
        // one. It is an `Area` like the label popup's: it lays itself out on the
        // frame the click landed on and draws on the next one, so the shapes the
        // rows are found in are the frame after that.
        let open_menu = |ctx: &egui::Context, app: &mut KimoApp| {
            let cell = titlebar_filter_rect(ctx, app, 1400.0);
            click_at(ctx, app, cell.center());
            let _ = frame_shapes_at(ctx, app, 1400.0);
            frame_shapes_at(ctx, app, 1400.0)
        };
        // The menu hangs under the cell, at the left of the window rather than
        // in the middle column: its rows are found where they are drawn — and
        // they stay there, because a tick does not close the menu.
        let menu_area = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
        let tick = |ctx: &egui::Context, app: &mut KimoApp, label: &str| {
            let shapes = frame_shapes_at(ctx, app, 1400.0);
            let row = text_boxes_of(&shapes, menu_area, label);
            assert_eq!(row.len(), 1, "the menu draws one {label} row");
            click_at(ctx, app, row[0].center());
        };
        let shapes = open_menu(&ctx, &mut app);
        assert_eq!(
            text_boxes_of(&shapes, menu_area, "All").len(),
            1,
            "the menu's way out of every view"
        );
        for filter in ListFilter::ALL {
            assert_eq!(
                text_boxes_of(&shapes, menu_area, filter.label()).len(),
                1,
                "the menu offers {}",
                filter.label()
            );
        }

        // A row is its view's own mark at the left, the name, and the tick at its
        // *end* — where a menu's check belongs — and nothing is ticked yet.
        let row_parts = |shapes: &[egui::epaint::ClippedShape], label: &str| {
            let text = text_boxes_of(shapes, menu_area, label)[0];
            // The row's own ends, measured from where its name starts: the mark
            // is the inset, the icon and the gap to the left of it, and the
            // popup's width reaches its right end.
            let row_left = text.left() - (FILTER_ROW_INSET + FILTER_ICON_SIZE + FILTER_ICON_GAP);
            let icon = egui::Rect::from_min_max(
                egui::pos2(row_left, text.top() - 4.0),
                egui::pos2(text.left() - 2.0, text.bottom() + 4.0),
            );
            let tail = egui::Rect::from_min_max(
                egui::pos2(text.right() + 2.0, text.top() - 4.0),
                egui::pos2(row_left + LIST_MENU_WIDTH, text.bottom() + 4.0),
            );
            (icon, tail)
        };
        for filter in ListFilter::ALL {
            let (icon, tail) = row_parts(&shapes, filter.label());
            assert!(
                !glyph_colors_in(&shapes, icon).is_empty(),
                "the {} row carries a mark of its own",
                filter.label()
            );
            assert!(
                glyph_colors_in(&shapes, tail).is_empty(),
                "and no tick while it is off: {:?}",
                glyph_colors_in(&shapes, tail)
            );
        }

        // Unread: the read mail is not in the list any more.
        tick(&ctx, &mut app, "Unread");
        assert!(app.list_filters.contains(ListFilter::Unread));
        let scheme = theme::of(&ctx);
        // The menu is an `Area` that fades in, and a shape's colour carries that
        // fade: the tick's own colour is the one the frame after it draws.
        for _ in 0..16 {
            run_app_frame(&ctx, &mut app, Vec::new());
        }
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let (_, tail) = row_parts(&shapes, "Unread");
        assert!(
            glyph_colors_in(&shapes, tail).contains(&scheme.accent),
            "the tick is at the end of the row, in the accent: {:?}",
            glyph_colors_in(&shapes, tail)
        );
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            drawn(&texts, "0") && drawn(&texts, "2") && !drawn(&texts, "1"),
            "the read mail is filtered out: {texts:?}"
        );

        // Flagged as well: the views are an AND, so what is left is the one mail
        // that is both — unread *and* flagged, which is also the one with a file.
        tick(&ctx, &mut app, "Flagged");
        assert!(
            app.list_filters.contains(ListFilter::Flagged)
                && app.list_filters.contains(ListFilter::Unread)
        );
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            drawn(&texts, "0") && !drawn(&texts, "1") && !drawn(&texts, "2"),
            "only the mail that passes both views is listed: {texts:?}"
        );

        // Unticking one widens the list again rather than switching views: the
        // flag is off, and what is left is the unread view — which the flagged
        // mail is still part of, with the other unread one.
        tick(&ctx, &mut app, "Flagged");
        assert!(!app.list_filters.contains(ListFilter::Flagged));
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            drawn(&texts, "0") && drawn(&texts, "2") && !drawn(&texts, "1"),
            "the unread view alone: the read mail is still out: {texts:?}"
        );

        // And "All" is the way out of every view at once.
        tick(&ctx, &mut app, "All");
        assert!(app.list_filters.is_all());
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            drawn(&texts, "0") && drawn(&texts, "1") && drawn(&texts, "2"),
            "every mail is back: {texts:?}"
        );
    }

    /// The sort cell sits after the filter and opens the menu's two groups: the
    /// order — newest on top, or oldest — and the key — date, sender, subject.
    /// Each is a choice of one, ticked at the end of the row in force, and the
    /// list is drawn in the order the two add up to.
    #[test]
    fn the_sort_cell_orders_the_list() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        app.selected_message = None;
        // Three mails whose three keys all disagree, so that which one the list
        // was sorted by is what the rows say. By date: Zebra (300), Apple (200),
        // Mango (100); by sender: Cleo, Bob, Ada; by subject: Apple, Mango,
        // Zebra.
        {
            let state = app.states.get_mut("acct").expect("the fixture account");
            let mails = &mut state.mailbox.folders[0].messages;
            for (index, (ts, from, subject)) in [
                (100, "Cleo <cleo@example.com>", "Mango"),
                (200, "Ada <ada@example.com>", "Apple"),
                (300, "Bob <bob@example.com>", "Zebra"),
            ]
            .into_iter()
            .enumerate()
            {
                mails[index].timestamp = Some(ts);
                mails[index].from = from.to_string();
                mails[index].subject = subject.to_string();
            }
            state.folders_changed();
        }
        // What the list drew, in the order it drew it. Each subject is one run
        // per row — an unread row's text is struck twice, half a point apart, so
        // the frame draws it twice — and the pane has no mail of its own on it to
        // draw a third.
        let drawn = |texts: &[String]| -> Vec<String> {
            let mut order: Vec<String> = Vec::new();
            for run in texts {
                if matches!(run.as_str(), "Mango" | "Apple" | "Zebra") && !order.contains(run) {
                    order.push(run.clone());
                }
            }
            order
        };

        let texts = frame_texts(&ctx, &mut app);
        assert_eq!(
            drawn(&texts),
            ["Zebra", "Apple", "Mango"],
            "the default: newest first, by date"
        );

        // The menu, in the two groups the cell promises.
        let menu_area = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
        let open_sort = |ctx: &egui::Context, app: &mut KimoApp| {
            let cell = titlebar_sort_rect(ctx, app, 1400.0);
            click_at(ctx, app, cell.center());
            let _ = frame_shapes_at(ctx, app, 1400.0);
            frame_shapes_at(ctx, app, 1400.0)
        };
        let click_row = |ctx: &egui::Context, app: &mut KimoApp, label: &str| {
            let shapes = frame_shapes_at(ctx, app, 1400.0);
            let row = text_boxes_of(&shapes, menu_area, label);
            assert_eq!(row.len(), 1, "the menu draws one {label} row");
            click_at(ctx, app, row[0].center());
        };
        let shapes = open_sort(&ctx, &mut app);
        for text in [
            "Sort order",
            "Sort by",
            "Newest on top",
            "Oldest on top",
            "Date",
            "From",
            "Subject",
        ] {
            assert_eq!(
                text_boxes_of(&shapes, menu_area, text).len(),
                1,
                "the menu draws {text}"
            );
        }
        for row in ["Newest on top", "Oldest on top", "Date", "From", "Subject"] {
            let text = text_boxes_of(&shapes, menu_area, row)[0];
            let icon = egui::Rect::from_min_max(
                egui::pos2(text.left() - 40.0, text.top() - 4.0),
                egui::pos2(text.left() - 2.0, text.bottom() + 4.0),
            );
            assert!(
                !glyph_colors_in(&shapes, icon).is_empty(),
                "the {row} row carries a mark of its own"
            );
        }

        // Oldest on top: the same three mails, the other way round.
        click_row(&ctx, &mut app, "Oldest on top");
        assert_eq!(app.list_sort.order, SortOrder::Oldest);
        let texts = frame_texts(&ctx, &mut app);
        assert_eq!(drawn(&texts), ["Mango", "Apple", "Zebra"], "oldest first");

        // By sender, with "newest on top" back on: the order group is what turns
        // the key round, so the names run Z to A.
        click_row(&ctx, &mut app, "Newest on top");
        click_row(&ctx, &mut app, "From");
        assert_eq!(app.list_sort.by, SortBy::From);
        let texts = frame_texts(&ctx, &mut app);
        assert_eq!(
            drawn(&texts),
            ["Mango", "Zebra", "Apple"],
            "Cleo, Bob, Ada: the reverse of the names"
        );

        // And oldest first: the names themselves.
        click_row(&ctx, &mut app, "Oldest on top");
        let texts = frame_texts(&ctx, &mut app);
        assert_eq!(drawn(&texts), ["Apple", "Zebra", "Mango"], "Ada, Bob, Cleo");

        // By subject, the last of the three keys.
        click_row(&ctx, &mut app, "Subject");
        assert_eq!(app.list_sort.by, SortBy::Subject);
        let texts = frame_texts(&ctx, &mut app);
        assert_eq!(drawn(&texts), ["Apple", "Mango", "Zebra"], "A to Z");

        // The row in force in each group carries the tick at its end, and the
        // others do not: the menu says what the list is doing.
        let scheme = theme::of(&ctx);
        for _ in 0..16 {
            run_app_frame(&ctx, &mut app, Vec::new());
        }
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let tail = |label: &str| {
            let text = text_boxes_of(&shapes, menu_area, label)[0];
            let row_left = text.left() - (FILTER_ROW_INSET + FILTER_ICON_SIZE + FILTER_ICON_GAP);
            egui::Rect::from_min_max(
                egui::pos2(text.right() + 2.0, text.top() - 4.0),
                egui::pos2(row_left + LIST_MENU_WIDTH, text.bottom() + 4.0),
            )
        };
        for (row, ticked) in [
            ("Oldest on top", true),
            ("Subject", true),
            ("Newest on top", false),
            ("Date", false),
            ("From", false),
        ] {
            let colors = glyph_colors_in(&shapes, tail(row));
            assert_eq!(
                colors.contains(&scheme.accent),
                ticked,
                "the {row} row's tick: {colors:?}"
            );
        }
    }

    /// The sort the menu asks for is one comparison: the three keys, either way
    /// round, and where the mail sits as the tie-break — so two mails equal by
    /// the key keep the folder's order rather than swapping about.
    #[test]
    fn the_list_sort_compares_by_its_key_and_stays_stable() {
        let key = |timestamp, from: &'static str, subject: &'static str, position| SortKey {
            timestamp,
            from,
            subject,
            position,
        };
        let newest_date = ListSort::default();
        assert_eq!(newest_date.order, SortOrder::Newest);
        assert_eq!(newest_date.by, SortBy::Date);
        // Newest first: the later date runs before the earlier one, and a mail
        // whose date would not parse is the oldest there is.
        assert_eq!(
            newest_date.compare(key(Some(200), "a", "a", 0), key(Some(100), "a", "a", 1)),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            newest_date.compare(key(None, "a", "a", 0), key(Some(1), "a", "a", 1)),
            std::cmp::Ordering::Greater
        );
        // The order group turns whatever key round.
        let oldest = ListSort {
            order: SortOrder::Oldest,
            ..ListSort::default()
        };
        assert_eq!(
            oldest.compare(key(Some(200), "a", "a", 0), key(Some(100), "a", "a", 1)),
            std::cmp::Ordering::Greater
        );
        // By sender and by subject: display names, compared as a reader would —
        // case-insensitively.
        let by_from = ListSort {
            by: SortBy::From,
            ..ListSort::default()
        };
        assert_eq!(
            by_from.compare(
                key(Some(1), "Ada <a@x>", "z", 0),
                key(Some(2), "bob <b@x>", "a", 1)
            ),
            std::cmp::Ordering::Greater,
            "newest first reverses the names too: Ada runs after bob"
        );
        let by_from_ascending = ListSort {
            order: SortOrder::Oldest,
            ..by_from
        };
        assert_eq!(
            by_from_ascending.compare(
                key(Some(1), "Ada <a@x>", "z", 0),
                key(Some(2), "bob <b@x>", "a", 1)
            ),
            std::cmp::Ordering::Less,
            "and oldest first is the names themselves, case-insensitively"
        );
        let by_subject = ListSort {
            by: SortBy::Subject,
            ..ListSort::default()
        };
        assert_eq!(
            by_subject.compare(key(Some(2), "a", "Apple", 0), key(Some(1), "b", "Mango", 1)),
            std::cmp::Ordering::Greater,
            "the same reversal, by subject"
        );
        let by_subject_ascending = ListSort {
            order: SortOrder::Oldest,
            ..by_subject
        };
        assert_eq!(
            by_subject_ascending
                .compare(key(Some(2), "a", "Apple", 0), key(Some(1), "b", "Mango", 1)),
            std::cmp::Ordering::Less,
            "Apple before Mango, oldest first"
        );
        // Equal by the key: where the mail sits decides, whichever way the sort
        // runs — which is what keeps the list still between two frames.
        for sort in [newest_date, oldest, by_from, by_subject] {
            assert_eq!(
                sort.compare(key(Some(5), "a", "a", 3), key(Some(5), "a", "a", 7)),
                std::cmp::Ordering::Less,
                "{sort:?}"
            );
        }
    }

    /// A file dropped on the compose card is attached to the draft: the card
    /// draws the name and the size, a path typed into its box is the other way
    /// in, a path that is not a file says so rather than attaching nothing, and
    /// a row's own Remove takes that one back off.
    #[test]
    fn a_file_dropped_on_the_compose_card_is_attached() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.open_compose();
        // The card is an `Area` like every other popup: it lays itself out on
        // the frame it is opened on, so the frame a drop lands in is the next
        // one.
        run_app_frame(&ctx, &mut app, Vec::new());

        let dir = std::env::temp_dir().join("kimo-compose-drop");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.pdf");
        std::fs::write(&path, b"a report").unwrap();

        drop_files(&ctx, &mut app, vec![path.clone()]);
        let draft = app.compose.as_ref().expect("the card is open");
        assert_eq!(draft.attachments.len(), 1, "the drop attached it");
        assert_eq!(draft.attachments[0].name, "report.pdf");
        assert!(draft.attach_error.is_none(), "{:?}", draft.attach_error);

        // The card says what it holds.
        let texts = frame_texts(&ctx, &mut app);
        assert!(texts.iter().any(|run| run == "report.pdf"), "{texts:?}");
        assert!(
            texts.iter().any(|run| run == "application/pdf · 8 B"),
            "the card's second line, as the reading pane draws it: {texts:?}"
        );

        // The card that says what to do with a file is one area, and it says both
        // ways in: a click opens the desktop's dialog, and a file dragged from a
        // file manager lands on the window and is attached by `attach_dropped`.
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts
                .iter()
                .any(|run| run == "Click to select files, or drag files here"),
            "{texts:?}"
        );
        // And while the desktop's dialog is open, the card says so — and nothing
        // else on it can be pressed.
        app.picking_files = true;
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts.iter().any(|run| run == "Choosing files…"),
            "{texts:?}"
        );
        app.picking_files = false;

        // A path that is not a file says so — under the card, where the reason
        // for the last file that did not attach is drawn — and attaches nothing.
        // (Reached through `attach_file` rather than by clicking the card: a
        // click opens the desktop's dialog, which is not a test's business.)
        let missing = dir.join("gone.pdf");
        let draft = app.compose.as_mut().expect("open");
        assert!(!attach_file(draft, missing.clone()), "nothing was attached");
        assert_eq!(draft.attachments.len(), 1, "the file that was there stays");
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts.iter().any(|run| run.contains("gone.pdf")),
            "the reason names the path: {texts:?}"
        );

        // And a path that is a file goes on: the same file twice is two parts, in
        // the order they were attached.
        let draft = app.compose.as_mut().expect("open");
        assert!(attach_file(draft, path.clone()), "the file attaches");
        assert_eq!(draft.attachments.len(), 2);
        assert!(draft.attach_error.is_none(), "and the refusal is gone");

        // Every file card carries a small cross in its right-hand gutter — two
        // strokes whose midpoints are the button's centre — and a click on the
        // cross takes that file off the mail. It is found by the strokes it drew
        // rather than by working out where it ought to be.
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let area = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
        let cards = attachment_card_rects(&shapes, area);
        assert_eq!(cards.len(), 2, "one card per file: {cards:?}");
        let centres = stroke_centres(&shapes, area);
        let crosses: Vec<egui::Pos2> = cards
            .iter()
            .filter_map(|card| {
                let wanted = egui::pos2(
                    card.right() - ATTACHMENT_CARD_PADDING - ATTACHMENT_CLOSE_SIZE / 2.0,
                    card.center().y,
                );
                centres
                    .iter()
                    .copied()
                    .find(|centre| centre.distance(wanted) < 1.0)
            })
            .collect();
        assert_eq!(crosses.len(), 2, "a cross on each card: {centres:?}");

        click_at(&ctx, &mut app, crosses[0]);
        assert_eq!(
            app.compose.as_ref().expect("open").attachments.len(),
            1,
            "the cross took that one off, and the other stays"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The four views ask a row the four things a list can answer, and "to me"
    /// reads the `To` header as the sender wrote it — the address is somewhere
    /// in it, whatever else is there — against the accounts' own addresses. The
    /// set they go into is the AND the menu promises.
    #[test]
    fn the_list_filter_predicates() {
        let me = vec!["me@example.com".to_string()];
        let facts = |seen, flagged, to: &'static str, attachments| FilterFacts {
            seen,
            flagged,
            to,
            attachments,
        };
        assert!(ListFilter::Unread.matches(facts(false, false, "x@y", 0), &me));
        assert!(!ListFilter::Unread.matches(facts(true, false, "x@y", 0), &me));
        assert!(ListFilter::Flagged.matches(facts(true, true, "x@y", 0), &me));
        assert!(!ListFilter::Flagged.matches(facts(true, false, "x@y", 0), &me));
        assert!(ListFilter::HasFiles.matches(facts(true, false, "x@y", 2), &me));
        assert!(!ListFilter::HasFiles.matches(facts(true, false, "x@y", 0), &me));
        assert!(ListFilter::ToMe.matches(facts(true, false, "Ada <ME@example.com>", 0), &me));
        assert!(ListFilter::ToMe.matches(facts(true, false, "a@x, me@example.com", 0), &me));
        assert!(!ListFilter::ToMe.matches(facts(true, false, "someone@elsewhere.com", 0), &me));
        // An account with no address of its own leaves the question unanswerable
        // — which is what the menu row says rather than filtering to nothing.
        assert!(!ListFilter::ToMe.matches(facts(true, false, "me@example.com", 0), &[]));
        for filter in ListFilter::ALL {
            assert!(!filter.label().is_empty());
            assert!(!filter.note().is_empty());
        }

        // The set they go into is the AND the menu promises: an empty one is
        // every mail, each tick narrows it further, and unticking widens it
        // again without disturbing the views that are left.
        let mut filters = ListFilters::default();
        assert!(filters.is_all());
        assert!(filters.matches(facts(true, false, "x@y", 0), &me));
        filters.toggle(ListFilter::Unread);
        filters.toggle(ListFilter::HasFiles);
        assert!(!filters.is_all());
        assert!(filters.contains(ListFilter::Unread) && filters.contains(ListFilter::HasFiles));
        assert!(filters.matches(facts(false, false, "x@y", 1), &me));
        assert!(
            !filters.matches(facts(false, false, "x@y", 0), &me),
            "unread but with nothing attached"
        );
        assert!(
            !filters.matches(facts(true, false, "x@y", 1), &me),
            "attached but read"
        );
        filters.toggle(ListFilter::Unread);
        assert!(!filters.contains(ListFilter::Unread));
        assert!(filters.matches(facts(true, false, "x@y", 1), &me));
        // The set keeps the menu's order, whatever order the boxes were ticked.
        filters.toggle(ListFilter::ToMe);
        filters.toggle(ListFilter::Unread);
        assert_eq!(
            filters.0,
            vec![ListFilter::Unread, ListFilter::ToMe, ListFilter::HasFiles]
        );
        filters.clear();
        assert!(filters.is_all());
    }

    /// A view that hides every mail leaves the column saying so: the list is
    /// empty because of the filter, which is not the same thing as a folder with
    /// no mail in it.
    #[test]
    fn a_filter_that_matches_nothing_says_so() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 2);
        app.set_list_filters(ListFilters(vec![ListFilter::Flagged]));
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts
                .iter()
                .any(|run| run == "No mail matches this filter."),
            "the list says why it is empty: {texts:?}"
        );
        assert!(
            !texts.iter().any(|run| run == "Subject 0"),
            "and draws no row at all: {texts:?}"
        );

        // The same for a search's hits, which the column's filter narrows
        // without touching the query: it says the hit was filtered out, not that
        // nothing matched.
        app.set_list_filters(ListFilters::default());
        app.search.text = "report".to_string();
        app.search_hits = vec![SearchHit {
            account_id: "acct".to_string(),
            folder: "INBOX".to_string(),
            uid: "0".to_string(),
            subject: "Subject 0".to_string(),
            from: "Ada <ada@example.com>".to_string(),
            to: "someone@elsewhere.com".to_string(),
            date: "2026-01-01".to_string(),
            timestamp: Some(1_700_000_000),
            seen: false,
            flagged: false,
            attachments: 0,
            snippet: "a line with report in it".to_string(),
        }];
        app.set_list_filters(ListFilters(vec![ListFilter::HasFiles]));
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts.iter().any(|run| run == "No hit matches this filter."),
            "a filtered-out hit is not an empty query: {texts:?}"
        );
    }

    /// The title bar's flag cell is the one mail action with a state of its
    /// own: a click marks the mail the pane shows, the mark is the warning fill
    /// the mail's **row** takes — the reading pane is the mail's own surface and
    /// is not tinted — the cell's own glyph turns the scheme's warning colour
    /// while the flag stands, so the cell says which of the two things a click
    /// on it would do, and the same click takes the flag off again.
    #[test]
    fn the_flag_cell_marks_the_mail_it_is_on() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        let scheme = theme::of(&ctx);
        // The warning fill the list draws, told apart from the chrome: a row is
        // the only thing in the column that tall.
        let row_fill = |shapes: &[egui::epaint::ClippedShape], pane_left: f32| {
            fills_in_color(shapes, theme::of(&ctx).flag)
                .into_iter()
                .find(|rect| rect.height() >= MESSAGE_ROW_HEIGHT && rect.center().x < pane_left)
        };

        // Mail "1" is the one to mark, and the pane opens on it: the cell is
        // about the mail the pane shows.
        app.selected_message = Some("1".to_string());
        // The run fades in with the mail it acts on (`MAIL_ACTION_FADE_TIME`),
        // and what is asked below is the cell at rest — the glyph's own colour
        // rather than a faded one.
        for _ in 0..16 {
            run_app_frame(&ctx, &mut app, Vec::new());
        }
        assert!(
            !app.selected_mail().expect("a mail is open").flagged,
            "a mail starts unflagged"
        );
        let cell = egui::Rect::from_center_size(
            titlebar_action_center(&ctx, &mut app, MailAction::Flag, 1400.0),
            egui::Vec2::splat(TITLEBAR_BUTTON_SIZE),
        );
        let pane_left = app.list_column.right();
        click_at(&ctx, &mut app, cell.center());
        assert!(
            app.selected_mail().expect("a mail is open").flagged,
            "the cell marked the mail the pane shows"
        );

        // The cell draws its glyph in the warning colour rather than in the
        // bar's text colour — and the pane is still the mail's own surface: the
        // mark is shown in the list, not painted on the mail.
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let colors = glyph_colors_in(&shapes, cell);
        assert!(
            colors.contains(&scheme.warn),
            "the flag cell draws its glyph in the warning colour: {colors:?}"
        );
        assert!(
            fills_in_color(&shapes, scheme.flag)
                .into_iter()
                .all(|rect| rect.center().x < pane_left),
            "no fill of the warning colour is on the pane's side of the splitter"
        );

        // Another mail is open: the flagged one — right there in the list,
        // unselected — is the row that takes the warning fill.
        app.selected_message = Some("0".to_string());
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let row = row_fill(&shapes, pane_left).expect("the flagged mail's row is filled");
        assert_eq!(
            row.top(),
            TITLEBAR_HEIGHT + MESSAGE_ROW_HEIGHT,
            "and it is the flagged mail's row, not the open one's"
        );

        // The click is a toggle, and it is about the mail the pane shows: with
        // the flagged mail open again, the same cell takes the mark off.
        app.selected_message = Some("1".to_string());
        for _ in 0..16 {
            run_app_frame(&ctx, &mut app, Vec::new());
        }
        click_at(&ctx, &mut app, cell.center());
        assert!(
            !app.selected_mail().expect("a mail is open").flagged,
            "the cell took the flag off again"
        );
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        assert!(
            row_fill(&shapes, pane_left).is_none(),
            "and the warning fill went with it"
        );
    }

    /// A flagged mail's row carries the mark itself rather than only the fill:
    /// the flag glyph at the end of its title line, in the mark's own warning
    /// colour — and for a mail that also has parts, beside its paperclip rather
    /// than on top of it, since the row has room for both.
    #[test]
    fn a_flagged_row_draws_the_flag_beside_its_paperclip() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 2);
        let scheme = theme::of(&ctx);
        let key = MailKey::new("acct", "INBOX", "1");
        // The pane opens on mail "0", so the row asked about — the second one —
        // is drawn without the selection's own fill.
        app.selected_message = Some("0".to_string());
        let row = |app: &KimoApp| {
            egui::Rect::from_min_size(
                egui::pos2(app.list_column.left(), TITLEBAR_HEIGHT + MESSAGE_ROW_HEIGHT),
                egui::vec2(app.list_column.width(), MESSAGE_ROW_HEIGHT),
            )
        };
        let marks = |app: &mut KimoApp| {
            let shapes = frame_shapes_at(&ctx, app, 1400.0);
            let row = row(app);
            let colors = glyph_colors_in(&shapes, row);
            let bounds = list_glyph_strokes(&shapes, app.list_column)
                .into_iter()
                .filter(|stroke| row.contains(stroke.center()))
                .fold(None::<egui::Rect>, |bounds, stroke| {
                    Some(bounds.map_or(stroke, |b| b.union(stroke)))
                });
            (colors, bounds, row)
        };

        // Nothing marked: no flag glyph on the row at all.
        let (colors, bounds, _) = marks(&mut app);
        assert!(
            !colors.contains(&scheme.warn) && bounds.is_none(),
            "an unmarked row draws no flag: {colors:?} {bounds:?}"
        );

        // Marked: the flag is drawn, in the warning colour, at the end of the
        // row's title line.
        app.set_flag_on_mail(&key, true);
        let (colors, bounds, row) = marks(&mut app);
        assert!(
            colors.contains(&scheme.warn),
            "the flagged row draws its flag in the warning colour: {colors:?}"
        );
        let flag = bounds.expect("the flagged row draws a glyph");
        assert!(
            flag.width() > ROW_FLAG_WIDTH - 2.0
                && flag.width() <= ROW_FLAG_SIZE
                && flag.height() > ROW_FLAG_SIZE - 6.0
                && flag.height() <= ROW_FLAG_SIZE,
            "the flag fills the box the row reserves for it: {flag:?}"
        );
        assert!(
            flag.right() > row.right() - 30.0 && flag.right() <= row.right(),
            "the flag sits at the end of the title line: {flag:?} in {row:?}"
        );

        // A mail with a part as well: the clip is the outermost of the two, so
        // the flag's own room is a clip's width further in — and the row draws
        // both marks, each in its own colour.
        let state = app.states.get_mut("acct").expect("the fixture account");
        state.mailbox.folders[0].messages[1].attachments = vec![Attachment {
            name: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size: 1024,
        }];
        state.folders_changed();
        let (colors, bounds, _) = marks(&mut app);
        assert!(
            colors.contains(&scheme.warn) && colors.contains(&scheme.text_weak),
            "the row draws both marks, each in its own colour: {colors:?}"
        );
        let both = bounds.expect("the row draws both glyphs");
        assert!(
            both.width() > flag.width() + ROW_CLIP_GAP,
            "the clip sits beside the flag rather than over it: {both:?} against {flag:?}"
        );
    }

    /// The title bar's own label cell opens the same popup, about the mail the
    /// reading pane shows: the bar answers the pane's mail, the card head the
    /// mail it sits on.
    #[test]
    fn the_title_bars_label_cell_opens_the_popup_too() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);
        app.selected_message = Some("0".to_string());
        run_app_frame(&ctx, &mut app, Vec::new());

        let centre = titlebar_action_center(&ctx, &mut app, MailAction::Label, 1400.0);
        // The bar drew the cell where the arithmetic says (the fill under the
        // pointer is the cell's own), and the click opens the popup.
        assert!(
            fill_under_the_pointer(&ctx, &mut app, 1400.0, centre, TITLEBAR_BUTTON_SIZE).is_some(),
            "no label cell at {centre:?}"
        );
        click_at(&ctx, &mut app, centre);
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts
                .iter()
                .any(|run| run == "No labels yet — name one below."),
            "the bar's label cell opened the popup: {texts:?}"
        );
    }

    /// A mail's card says what it is labelled with, under its head: one chip per
    /// label, as wide as the name wants. A mail nobody labelled carries no row at
    /// all, so the head is still the first thing on it.
    #[test]
    fn a_mails_card_says_what_it_is_labelled_with() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 2);
        app.selected_message = Some("0".to_string());
        let pane = |app: &KimoApp| {
            egui::Rect::from_min_max(
                egui::pos2(app.list_column.right(), 0.0),
                egui::pos2(1400.0, 900.0),
            )
        };

        // Nothing labelled: no chips, and the subject follows the head row.
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        assert!(text_boxes_of(&shapes, pane(&app), "Work").is_empty());

        label_mail(&mut app, "acct", "INBOX", "0", "Work");
        let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
        let name = text_boxes_of(&shapes, pane(&app), "Work")
            .pop()
            .expect("the chip's name is on the card");
        let head_bottom = TITLEBAR_HEIGHT
            + CONTENT_GUTTER
            + CONTENT_PADDING.y
            + AVATAR_DIAMETER.max(CARD_ACTION_SIZE);
        assert!(
            name.top() >= head_bottom,
            "the chips go under the head row: {name:?} against {head_bottom}"
        );
        let scheme = theme::of(&ctx);
        let chip = fills_in_color(&shapes, scheme.hover)
            .into_iter()
            .find(|rect| rect.contains_rect(name))
            .unwrap_or_else(|| panic!("nothing filled under {name:?}"));
        assert!((chip.height() - LABEL_CHIP_HEIGHT).abs() < 0.5, "{chip:?}");
        assert!(
            (chip.width() - (name.width() + LABEL_CHIP_PADDING * 2.0)).abs() < 0.5,
            "the chip is the name and its padding: {chip:?}"
        );
    }
}
