//! Compose drafts and the two containers they open in: the popup window and
//! the card under the mail it answers, plus sending and attachment helpers.

use super::*;

/// The account a new draft is written from: the one whose mail is open, or the
/// first configured account when none is. An id that is no longer configured —
/// the account was removed with a draft open — falls back the same way, so the
/// From list never shows an account that is not there.
pub(super) fn compose_from(accounts: &[Account], selected: Option<&str>) -> Option<String> {
    selected
        .filter(|id| accounts.iter().any(|account| account.id == *id))
        .map(str::to_string)
        .or_else(|| accounts.first().map(|account| account.id.clone()))
}
/// The subject of a mail that answers or passes on another: the prefix,
/// unless the subject already carries one — `Re: Re:` helps nobody, and the
/// prefixes other clients write (`答复:`, `FWD:`) are prefixes all the same
/// (`threads::strip_subject_prefix`, the threading's own table).
pub(super) fn answer_subject(prefix: &str, subject: &str) -> String {
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
pub(super) fn attach_file(draft: &mut ComposeDraft, path: PathBuf) -> bool {
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
pub(super) fn quoted_body(mail: &MailMessage) -> String {
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
pub(super) fn send_blocker(
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
pub(super) fn sent_notice(recipients: &[String], copy: &SentCopy) -> String {
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
/// The compose modal: the width of its card (what the boxes are laid out for),
/// the label column of its header fields, the height of one of those one-line
/// fields, and the height of the body box, which scrolls rather than growing
/// the card.
pub(super) const COMPOSE_WIDTH: f32 = 560.0;
pub(super) const COMPOSE_LABEL_WIDTH: f32 = 56.0;
/// The footer buttons the on-the-spot card draws: an icon and a word in one
/// target, taller than a stock button and wider than its label needs. The
/// popup keeps egui's own two — its form is wide and its footer is a line of it,
/// while this card is a card and its two buttons are what it is for.
pub(super) const COMPOSE_BUTTON_HEIGHT: f32 = 34.0;
pub(super) const COMPOSE_BUTTON_PADDING: f32 = 12.0;
pub(super) const COMPOSE_BUTTON_ICON: f32 = 16.0;
pub(super) const COMPOSE_BUTTON_ICON_GAP: f32 = 8.0;
pub(super) const COMPOSE_BUTTON_FONT_SIZE: f32 = 13.0;
pub(super) const COMPOSE_BUTTON_CORNER_RADIUS: u8 = 6;
pub(super) const COMPOSE_FIELD_HEIGHT: f32 = 22.0;
pub(super) const COMPOSE_BODY_HEIGHT: f32 = 320.0;
/// The cell the reply's To row ends with on the spot: the chevron that folds
/// the Cc and Bcc lines away, and the square the pointer clicks it in — a
/// field's own height, so the row keeps its height whether they are out or
/// away — and the chevron drawn inside that cell.
pub(super) const COMPOSE_FOLD_CELL: f32 = COMPOSE_FIELD_HEIGHT;
pub(super) const COMPOSE_FOLD_GLYPH: f32 = 12.0;
/// The area the compose card's attach target takes: one line of text, the width
/// of the field column, and the corner radius the cards under it use.
pub(super) const ATTACH_CARD_HEIGHT: f32 = 46.0;
pub(super) const ATTACH_CARD_CORNER_RADIUS: u8 = 6;
pub(super) const ATTACH_CARD_TEXT_SIZE: f32 = 13.0;
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
pub(super) fn attach_card(
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
/// The row is the card's own in either container: no label beside it, and the
/// cards and the box across the card's whole width, under the body they go with
/// — the popup's form draws it the same way the answer on the spot does.
///
/// Returns the file a click took off, which the caller applies once the card
/// is drawn — the run must not be changed under the cards that are reading it
/// — and whether the dialog was asked for.
pub(super) fn compose_files_row(
    ui: &mut egui::Ui,
    draft: &ComposeDraft,
    width: f32,
    picking: bool,
    sending: bool,
) -> (Option<usize>, bool) {
    // While files are over the window the box says what a drop would do.
    let hovering = ui.ctx().input(|input| !input.raw.hovered_files.is_empty());
    let mut remove = None;
    let mut pick = false;
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
            "Choosing attachments…"
        } else if hovering {
            "Drop the files here"
        } else {
            "Click to select attachments, or drag files here"
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
pub(super) fn compose_body(
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
                        .frame(field_frame(FIELD_MARGIN))
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
/// One of the compose footer's own buttons, where the card has the room for
/// them: an icon, a word, and tall enough to be the thing the form points at.
/// `primary` is the accent-filled one — Send, because it is what the card is
/// for; the other is a quiet outline that turns the scheme's danger on hover,
/// the treatment a delete button gets everywhere else in the app. A disabled
/// button is the card's fill and a muted word: what it would do is in its
/// tooltip (`on_disabled_hover_text`, the caller's).
pub(super) fn compose_button(
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
    // The word in the same ink as the glyph. The galley was laid out in the
    // scheme's text colour — it is what the button's width is measured from —
    // and a galley's sections carry that colour, so the shape overrides it:
    // `Painter::galley`'s fallback colour only reaches text laid out without
    // one, and a hovered Discard's white (or a disabled Send's grey) would
    // never arrive.
    ui.painter().add(
        egui::epaint::TextShape::new(
            egui::pos2(
                glyph.right() + COMPOSE_BUTTON_ICON_GAP,
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            ink,
        )
        .with_override_text_color(ink),
    );
    if enabled {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}
/// The margin a field's text keeps: `TextEdit`'s own default, spelled out here
/// because the box is Kimo's (`Field`) and it is the frame that carries the
/// margin — a frame egui would otherwise build out of this very number.
pub(super) const FIELD_MARGIN: egui::Margin = egui::Margin::symmetric(4, 2);
/// The width of a field's box: the one point every hairline in the app is
/// drawn at, and the width egui gives a focused field's own frame.
pub(super) const FIELD_STROKE: f32 = 1.0;
/// The To row of either container: the label the container's rows wear
/// (`To` in the popup's form, `To:` on the letter under its mail), the field
/// in the box the caller names (`FieldBox` — flat on the spot, quiet in the
/// form), and the chevron at the row's end that folds the Cc and Bcc lines out
/// under it — down while they are away, up once they are under the row. The chevron's
/// cell is taken off the field's own width, so the row ends where every other
/// row's field does.
///
/// Returns the field's response — what puts the keyboard in it on the card's
/// first frame — and the chevron's, which is the caller's to act on.
pub(super) fn compose_to_row(
    ui: &mut egui::Ui,
    label: &str,
    to: &mut String,
    width: f32,
    sending: bool,
    open: bool,
    boxed: FieldBox,
) -> (egui::Response, egui::Response) {
    let scheme = theme::of(ui.ctx());
    ui.horizontal(|ui| {
        ui.add_sized(
            [COMPOSE_LABEL_WIDTH, COMPOSE_FIELD_HEIGHT],
            egui::Label::new(label).halign(egui::Align::RIGHT),
        );
        let room = COMPOSE_FOLD_CELL + ui.spacing().item_spacing.x;
        let edit = egui::TextEdit::singleline(to)
            .hint_text("name@example.com")
            .desired_width(width - room);
        let field = Field::new(edit).enabled(!sending);
        let field = match boxed {
            FieldBox::Boxed => field,
            FieldBox::Quiet => field.quiet(),
            FieldBox::Flat => field.flat(),
        };
        let field = ui.add(field);
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

impl KimoApp {
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
    pub(super) fn attach_dropped(&mut self) {
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
    pub(super) fn open_compose(&mut self) {
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
    pub(super) fn compose_about(
        &mut self,
        action: MailAction,
        spot: ComposeSpot,
        mail: &MailMessage,
    ) {
        let anchor = self.pane_mail_key(&mail.uid);
        self.compose_about_with(action, spot, mail, anchor);
    }
    /// Open a compose about a mail named by where it lives rather than by the
    /// pane — what a row's context menu does. The account the draft is *from*
    /// is the one the mail lives in, not whichever account the window happens
    /// to have open, and the draft is a popup: it was opened from a list row,
    /// not from a card in the pane.
    pub(super) fn compose_about_key(&mut self, action: MailAction, key: &MailKey) {
        let Some(mail) = self.mail_by_key(key) else {
            return;
        };
        self.compose_about_with(action, ComposeSpot::Popup, &mail, Some(key.clone()));
    }
    pub(super) fn compose_about_with(
        &mut self,
        action: MailAction,
        spot: ComposeSpot,
        mail: &MailMessage,
        anchor: Option<MailKey>,
    ) {
        if self.compose.as_ref().is_some_and(|draft| draft.sending) {
            return;
        }
        let account = anchor
            .as_ref()
            .map(|key| key.account.as_str())
            .or(self.selected_account.as_deref());
        let from = compose_from(&self.accounts, account);
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
        self.compose_anchor = anchor;
        self.compose = Some(draft);
        self.compose_spot = spot;
    }
    /// The mail the reading pane shows, for the actions about it — the title
    /// bar's and the header's. A uid the folder no longer holds is no mail
    /// at all, which is `has_selected_message`'s own question.
    pub(super) fn selected_mail(&self) -> Option<&MailMessage> {
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
    pub(super) fn compose_anchor_on_pane(&self) -> bool {
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
    /// Send the draft the compose card holds: the mail goes to a worker as a
    /// `Job::SendMessage`, and the card waits for the answer with its boxes and
    /// buttons disabled (`ComposeDraft::sending`).
    ///
    /// What closes the card is `Event::MessageSent`, not this: what has been sent
    /// is the mail the server accepted, and until it says so the draft is the
    /// only copy of it.
    pub(super) fn send_draft(&mut self) {
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
        // The window owns settings.json, so it is the one that says whether the
        // account's vendor preset is switched on; the worker applies the policy
        // of the vendor it is handed.
        let vendor = self.effective_vendor(&account);
        sync::spawn(
            Job::SendMessage {
                account,
                mail,
                vendor,
            },
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
    pub(super) fn compose_window(&mut self, ctx: &egui::Context) {
        let Some(heading) = self.compose.as_ref().map(|draft| draft.kind.label()) else {
            return;
        };
        let mut outcome = ComposeOutcome::Shown;
        let _ = egui::Modal::new(egui::Id::new("compose")).show(ctx, |ui| {
            ui.set_width(COMPOSE_WIDTH);
            ui.heading(heading);
            // No rule under the heading: the card below it is the same letter's
            // head the on-the-spot answer draws, and that head carries none.
            ui.add_space(4.0);
            outcome = self.compose_card(ui, COMPOSE_WIDTH, ComposeSpot::Popup);
        });
        self.settle_compose_outcome(outcome);
    }
    /// What the card's own buttons said, wherever the card was drawn: Discard
    /// closes the draft, Send hands it to a worker, and nothing is nothing.
    pub(super) fn settle_compose_outcome(&mut self, outcome: ComposeOutcome) {
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
    /// whose fields are flat, and the window's own form, whose fields are quiet
    /// (`FieldBox`, docs/ui-style.md).
    ///
    /// The draft is taken out for the card — the closure edits it while
    /// reading the account list — and put back unless the card was
    /// discarded. Send is returned for the caller to act on: the draft is
    /// back in place by then, which is what `send_draft` reads.
    pub(super) fn compose_card(
        &mut self,
        ui: &mut egui::Ui,
        width: f32,
        spot: ComposeSpot,
    ) -> ComposeOutcome {
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
        // The card's rows wear a mail header's labels, colons and all, in either
        // container: the popup and the answer under its mail draw the same head.
        let (from_label, to_label, cc_label, bcc_label, subject_label) =
            ("From:", "To:", "Cc:", "Bcc:", "Subject:");

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
            // constant: what the head's avatar column, the label column and the
            // gaps beside them leave.
            let spacing = ui.spacing().item_spacing.x;
            let field_width = width - AVATAR_DIAMETER - spacing - COMPOSE_LABEL_WIDTH - spacing;
            // One row of the card's form: the label in the card's own column,
            // and the field in the box the row asks for — Kimo's own, which is
            // where a field takes the accent (`Field`, `FieldBox`).
            let field_row = |ui: &mut egui::Ui,
                             label: &str,
                             value: &mut String,
                             hint: &str,
                             boxed: FieldBox| {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [COMPOSE_LABEL_WIDTH, COMPOSE_FIELD_HEIGHT],
                        egui::Label::new(label).halign(egui::Align::RIGHT),
                    );
                    let edit = egui::TextEdit::singleline(value)
                        .hint_text(hint)
                        .desired_width(field_width);
                    let field = Field::new(edit).enabled(!sending);
                    ui.add(match boxed {
                        FieldBox::Boxed => field,
                        FieldBox::Quiet => field.quiet(),
                        FieldBox::Flat => field.flat(),
                    })
                })
                .inner
            };
            // The row where the card is a letter rather than a form: flat, the
            // way the From list above it is drawn, without an outline or a fill.
            // Cc and Bcc are drawn with it when the To row's chevron has folded
            // the two out.
            let flat_field = |ui: &mut egui::Ui, label: &str, value: &mut String| {
                field_row(ui, label, value, "name@example.com", FieldBox::Flat)
            };
            // The form's rows, quiet: the input fill, and no line until the
            // pointer or the keyboard is on one (`docs/ui-style.md`).
            let quiet_field = |ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str| {
                field_row(ui, label, value, hint, FieldBox::Quiet)
            };
            // To, and the lines its chevron folds out under it, wherever the
            // card draws them — the head's own column on the spot, a row of the
            // popup's form there — so the fold is one thing in either container
            // (`ComposeDraft::copies_open`). The row is the container's: flat
            // and letter-labelled on the answer under its mail, quiet and
            // form-labelled in the popup.
            let mut to_and_copies = |ui: &mut egui::Ui| {
                let boxed = if inline {
                    FieldBox::Flat
                } else {
                    FieldBox::Quiet
                };
                let (to, fold) = compose_to_row(
                    ui,
                    to_label,
                    &mut draft.to,
                    field_width,
                    sending,
                    draft.copies_open,
                    boxed,
                );
                if fold.clicked() {
                    draft.copies_open = !draft.copies_open;
                }
                if first && !body_first {
                    to.request_focus();
                }
                if draft.copies_open {
                    if inline {
                        flat_field(ui, cc_label, &mut draft.cc);
                        flat_field(ui, bcc_label, &mut draft.bcc);
                    } else {
                        quiet_field(ui, cc_label, &mut draft.cc, "name@example.com");
                        quiet_field(ui, bcc_label, &mut draft.bcc, "name@example.com");
                    }
                }
            };

            // The From line, the card's own in either container: the row under
            // the account's face, labeled `From:`, and a list that opens rather
            // than a field to fill in (`flatten_widgets` takes egui's box off
            // it, box and all: an outline and a fill belong to a form, and this
            // row is a letter's line. Only its hover keeps a fill, which is what
            // still says it can be opened, and its chevron is painted where
            // egui's filled triangle would go).
            let fallback_from = compose_from(&self.accounts, self.selected_account.as_deref());
            let from_list = |ui: &mut egui::Ui, from: &mut Option<String>| {
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
                            flatten_widgets(ui, theme::of(ui.ctx()).hover);
                            // What egui's own icon hook is handed — where the
                            // filled triangle would go, and whether the list
                            // is open — is kept here for the chevron painted
                            // after the button: the pointer's state, which
                            // that chevron's colour answers, is known only
                            // then (`paint_combo_chevron`).
                            let icon = std::rc::Rc::new(std::cell::Cell::new((
                                egui::Rect::NOTHING,
                                false,
                            )));
                            let mut combo = egui::ComboBox::from_id_salt("compose-from")
                                .selected_text(label)
                                .width(field_width);
                            let hook = std::rc::Rc::clone(&icon);
                            combo = combo.icon(move |_, rect, _, is_open| {
                                hook.set((rect, is_open));
                            });
                            let list = combo.show_ui(ui, |ui| {
                                for (id, name, address) in &accounts {
                                    ui.selectable_value(
                                        from,
                                        Some(id.clone()),
                                        format!("{name} <{address}>"),
                                    );
                                }
                            });
                            let (place, open) = icon.get();
                            paint_combo_chevron(ui, &list.response, place, open);
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
            // The card's head, in two columns: the face of the account the mail
            // goes from on the left, and beside it the lines it goes out on —
            // From first, then To with the chevron that folds Cc and Bcc out
            // under it (`to_and_copies`), and Subject last on the popup's form,
            // the one container with a subject to name. No rule under the head,
            // in either container: these are the card's own first lines, and the
            // popup's heading is what sits over its form.
            let scheme = theme::of(ui.ctx());
            ui.horizontal_top(|ui| {
                let (slot, _) = ui
                    .allocate_exact_size(egui::Vec2::splat(AVATAR_DIAMETER), egui::Sense::hover());
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
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [COMPOSE_LABEL_WIDTH, COMPOSE_FIELD_HEIGHT],
                            egui::Label::new(from_label).halign(egui::Align::RIGHT),
                        );
                        from_list(ui, &mut draft.from);
                    });
                    to_and_copies(ui);
                    if !inline {
                        // The form's own last row: Subject, quiet like the
                        // lines above it (`docs/ui-style.md`).
                        quiet_field(ui, subject_label, &mut draft.subject, "Subject");
                    }
                });
            });

            // The card's writing and what goes with it, in the letter's order in
            // either container now: the body first — the mail is what the card
            // is for, and on the spot it is also what the mail above it is
            // answered with — and the attachments under it, across the card's
            // whole width with no label of their own.
            ui.add_space(6.0);
            let body = compose_body(ui, &mut draft.body, width, sending, !body_first);
            if first && body_first {
                body.request_focus();
            }
            ui.add_space(6.0);
            (remove, pick) = compose_files_row(ui, &draft, width, self.picking_files, sending);

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
            // The foot is the same in either container: no rule over it — the
            // card is a letter, and the mail above it, not a line, is what its
            // end is read against — what it has to say on a row of its own over
            // the buttons, and the card's own pair (`compose_button`) opening
            // the row at its left edge, the side the mail above is read from.
            if draft.error.is_some() || sending || blocker.is_some() {
                ui.horizontal(|ui| said(ui));
            }
            ui.horizontal(|ui| {
                // Discard first and Send beside it: an icon each and room around
                // them, Send in the accent because sending is what the card is
                // for, and Discard an outline that turns the scheme's danger on
                // hover — the mail it throws away is the only copy there is, and
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
}
