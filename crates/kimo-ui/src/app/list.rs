//! The message list: folder, search and label result lists; row/card
//! painting and hit-testing; selection, sorting, filtering, scrolling and the
//! older-mail button.

use super::*;

/// What a row's own delete button says: the mail the row speaks for, and — for
/// a conversation — that it is only that one, since a conversation's row shows
/// its newest mail and the button takes the same mail the row opens in the
/// pane. The wording is the title bar's Delete, which takes the same mail.
pub(super) fn row_delete_tooltip(place: RowPlace<'_>) -> &'static str {
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
pub(super) fn row_flag_tooltip(place: RowPlace<'_>, flagged: bool) -> &'static str {
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
pub(super) fn delete_selection_tooltip(count: usize) -> String {
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
pub(super) fn select_all_state(checked: usize, listed: usize) -> CheckState {
    match checked {
        0 => CheckState::Off,
        n if n >= listed => CheckState::On,
        _ => CheckState::Partial,
    }
}
/// The select-all row's label: the action a click would take, so the row says
/// what it is for without a tooltip having to be found.
pub(super) fn select_all_label(state: CheckState) -> &'static str {
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
pub(super) fn select_all_band(viewport: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(viewport.left(), viewport.top() + TITLEBAR_HEIGHT),
        egui::vec2(viewport.width(), SELECT_ALL_HEIGHT),
    )
}
/// The select-all row's hover text, which is where the count lives — the row
/// itself is one line of chrome and stays one line whatever the list holds.
pub(super) fn select_all_tooltip(state: CheckState, listed: usize) -> String {
    match (state, listed) {
        (CheckState::On, 1) => "Clear the selection — the mail in this list is checked".to_string(),
        (CheckState::On, n) => {
            format!("Clear the selection — all {n} mails in this list are checked")
        }
        (CheckState::Off | CheckState::Partial, 1) => "Select the mail in this list".to_string(),
        (CheckState::Off | CheckState::Partial, n) => format!("Select all {n} mails in this list"),
    }
}
/// One line standing above the message list's first row, where the rows are
/// padded for the title bar. `top` is that padding, which the second banner in
/// the column drops so the two stack; `color` is left to the label when a
/// banner is not a warning.
pub(super) fn list_banner(ui: &mut egui::Ui, top: i8, text: &str, color: Option<egui::Color32>) {
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
/// The floating `Load older mail` button: its size, how far its bottom edge
/// stays clear of the column's, and its corner (the same roundness as the
/// search box, so the two controls the bar and the list float read as one
/// family rather than as two widgets that happen to be on top).
pub(super) const LOAD_OLDER_WIDTH: f32 = 150.0;
pub(super) const LOAD_OLDER_HEIGHT: f32 = 26.0;
pub(super) const LOAD_OLDER_MARGIN: f32 = 12.0;
pub(super) const LOAD_OLDER_CORNER_RADIUS: u8 = SEARCH_BOX_CORNER_RADIUS;
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
pub(super) fn list_card(
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
pub(super) fn virtual_rows<R>(
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
pub(super) const MESSAGE_ROW_HEIGHT: f32 = 68.0;
/// The select-all row at the top of the list in select mode: its own height,
/// not a mail row's — it is one line of chrome with a box on it — as the
/// frame margin wants it (whole points, like `LIST_PADDING`) and as the
/// drawing wants it.
pub(super) const SELECT_ALL_HEIGHT_I8: i8 = 32;
pub(super) const SELECT_ALL_HEIGHT: f32 = SELECT_ALL_HEIGHT_I8 as f32;
pub(super) const AVATAR_DIAMETER: f32 = 32.0;
/// The room between an avatar and the name beside it — the message list's
/// own gap, kept by the pane's sender row so the two read alike.
pub(super) const AVATAR_TEXT_GAP: f32 = 10.0;
/// The checkbox a row draws where its avatar would be — select mode's control,
/// and the way into the mode — with the room its click target keeps around the
/// box on every side, which is also the rounded fill it shows under the
/// pointer. The box is not the avatar's size: it is a control, and an 18 pt box
/// in a 32 pt slot reads as one, while a 32 pt box would read as a button.
pub(super) const CHECKBOX_SIZE: f32 = 18.0;
pub(super) const CHECKBOX_PADDING: f32 = 5.0;
pub(super) const CHECKBOX_CORNER_RADIUS: u8 = 4;
/// The delete button a mail row offers under the pointer: the cell at the right
/// end of its first line — where the timestamp sits, which gives way to it —
/// and the glyph drawn in it. Smaller than the title bar's cells: a row's
/// control is a note on the mail, not chrome.
pub(super) const ROW_ACTION_SIZE: f32 = 24.0;
pub(super) const ROW_ACTION_GLYPH: f32 = 15.0;
pub(super) const ROW_ACTION_CORNER_RADIUS: u8 = 4;
/// The gap between a row's own cells — the flag and the delete — which is the
/// title bar's own gap: the pair is one run by the row's edge, the way the bar's
/// run and the mail head's are.
pub(super) const ROW_ACTION_GAP: f32 = 6.0;
/// The width of the check drawn on a checked box, and of the dash a partly
/// checked conversation gets.
pub(super) const CHECKBOX_STROKE: f32 = 2.0;
/// Font size of the avatar's initial — a bit under half the diameter.
pub(super) const AVATAR_FONT_SIZE: f32 = 14.0;
/// Font sizes of the row's three lines, in points: the timestamp, the title
/// line (sender over subject, the one that carries the unread weight) and the
/// muted preview under it.
pub(super) const ROW_DATE_FONT_SIZE: f32 = 11.0;
pub(super) const ROW_TITLE_FONT_SIZE: f32 = 12.0;
pub(super) const ROW_PREVIEW_FONT_SIZE: f32 = 11.0;
/// Where those three lines are centred, measured down from the row's top edge:
/// the sender and the timestamp, the subject under it, and the preview under
/// that. They are one block centred in the row — the subject line is the one
/// straddling the middle of `MESSAGE_ROW_HEIGHT` — which is what leaves the room
/// above the sender's line and the room below the preview the same; spacing the
/// lines from the row's top edge left the preview twice as close to the bottom
/// as the sender was to the top.
pub(super) const ROW_SENDER_Y: f32 = 15.0;
pub(super) const ROW_SUBJECT_Y: f32 = 35.0;
pub(super) const ROW_PREVIEW_Y: f32 = 52.0;
/// How far a conversation's own row — and the mails it holds — are inset from
/// the column's edge, and the room the chevron is drawn in at the left of that.
/// The two are one number: what the chevron is drawn in is exactly what the
/// thread's mails are inset by, so the block reads as one thing. Every row is
/// inset by it, a mail with nothing to open included, so the avatars line up
/// down the whole list.
pub(super) const THREAD_INDENT: f32 = 18.0;
pub(super) const THREAD_CHEVRON: f32 = 10.0;
/// How much further in a mail of an *open* conversation is drawn, on top of
/// `THREAD_INDENT`, and the avatar it is drawn with: a step and a smaller
/// circle instead of the guide line such a mail used to draw down the chevron's
/// column. The circle starts exactly `THREAD_STEP` further in, and the mail's
/// text `THREAD_STEP` + `THREAD_AVATAR_DIAMETER` - `AVATAR_DIAMETER` — the
/// run between the two circles, which is 12 pt: the smaller avatar gives part of
/// the step back. The step is the whole of the mark, so it has to be visible
/// without reading as a misalignment.
pub(super) const THREAD_STEP: f32 = 20.0;
pub(super) const THREAD_AVATAR_DIAMETER: f32 = 24.0;
/// Font size of that avatar's initial — a bit under half the diameter.
pub(super) const THREAD_AVATAR_FONT_SIZE: f32 = 11.0;
/// How much of the row's own weak color the chevron keeps at rest. It is a
/// quiet control at the edge of a row, and it comes up to the full color under
/// the pointer, which is what says it can be clicked.
pub(super) const THREAD_CHEVRON_ALPHA: f32 = 0.65;
/// The paperclip a row draws when the mail has parts: the box its outline is
/// drawn in (`paint_clip_icon` maps the glyph's 24-unit design onto it), the
/// width the clip actually comes out at — a clip is taller than it is wide, so
/// the box is square and the drawn glyph is not — and the room it keeps in
/// front of itself. The title line gives up the width and that room; the box's
/// own height is what keeps the glyph on the line.
pub(super) const ROW_CLIP_SIZE: f32 = 14.0;
pub(super) const ROW_CLIP_WIDTH: f32 = 8.5;
pub(super) const ROW_CLIP_GAP: f32 = 7.0;
/// The flag a marked mail draws at the end of its title line, in the slot the
/// paperclip shares with it. The size is the box the 24×24 outline is mapped
/// into, and the width is what that outline comes to at it — its pole and
/// banner span x 5 to 19 — which is the room the title leaves for it.
pub(super) const ROW_FLAG_SIZE: f32 = 14.0;
pub(super) const ROW_FLAG_WIDTH: f32 = (19.0 - 5.0) / 24.0 * ROW_FLAG_SIZE;
/// How far the second strike of bold text sits from the first, in points.
pub(super) const BOLD_STRIKE: f32 = 0.5;

impl KimoApp {
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
    pub(super) fn mail_list(&mut self, ui: &mut egui::Ui) {
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
    pub(super) fn folder_messages(&mut self, ui: &mut egui::Ui) -> Option<f32> {
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
        // A row's context menu named a mail and an action; both are applied
        // once the borrow of the state the rows came from is over.
        let mut menu: Option<(MailKey, MailMenuAction)> = None;
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
                                    // The row's context menu is about the mail
                                    // the row speaks for — its newest, the one
                                    // the row opens — and is a right-click of
                                    // its own, beside the row's left-click
                                    // actions.
                                    if let Some(action) =
                                        mail_context_menu(&drawn.row, conversation.flagged)
                                    {
                                        let newest = &messages[conversation.newest];
                                        menu = Some((
                                            MailKey::new(account, folder, &newest.uid),
                                            action,
                                        ));
                                    }
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
                                    if let Some(action) =
                                        mail_context_menu(&drawn.row, messages[index].flagged)
                                    {
                                        menu = Some((
                                            MailKey::new(account, folder, &messages[index].uid),
                                            action,
                                        ));
                                    }
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
        if let Some((key, action)) = menu {
            self.apply_mail_menu(key, action);
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
    /// The folder the list is showing, as `(account, folder)`: what a folder    /// The folder the list is showing, as `(account, folder)`: what a folder
    /// row's selection key and its checkbox id are built from. The rows are only
    /// drawn while a folder is open — the search results have a row of their
    /// own, `search_row`, which names the folder each hit lives in — so the two
    /// empty fallbacks describe a list that cannot be drawn anyway.
    pub(super) fn list_folder(&self) -> (&str, &str) {
        (
            self.selected_account.as_deref().unwrap_or_default(),
            self.selected_folder.as_deref().unwrap_or_default(),
        )
    }
    /// What a folder row's checkbox draws: `None` in view mode — where the
    /// avatar stands and the pointer alone brings an empty box over the row it
    /// is on — and otherwise the state of the mail the row stands for. `uids`
    /// is that mail, or every mail of the conversation the row speaks for.
    pub(super) fn folder_checkbox(&self, uids: &[&str]) -> Option<CheckState> {
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
    pub(super) fn hit_checkbox(&self, hit: &SearchHit) -> Option<CheckState> {
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
    pub(super) fn labelled_checkbox(&self, mail: &LabelledMail) -> Option<CheckState> {
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
    pub(super) fn check_state(&self, keys: &[MailKey]) -> CheckState {
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
    pub(super) fn toggle_check(&mut self, keys: Vec<MailKey>) {
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
    pub(super) fn cancel_select(&mut self) {
        self.select_mode = false;
        self.selected_mails.clear();
    }
    /// Check every mail of the list, or clear the selection when it is all in:
    /// the select-all row's click. Clearing is the one answer that can never
    /// leave a key behind for a row that is not on screen — and the mode stays
    /// on, exactly as it does when the last box is unchecked by hand.
    pub(super) fn toggle_all(&mut self, keys: Vec<MailKey>) {
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
    pub(super) fn select_all_row(
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
    pub(super) fn load_older_button(&mut self, ui: &mut egui::Ui) {
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
    pub(super) fn folder_messages_of(
        &self,
        account_id: &str,
        folder: &str,
    ) -> Option<&[MailMessage]> {
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
    pub(super) fn search_results(&mut self, ui: &mut egui::Ui) -> Option<f32> {
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
        // A hit's context menu named a mail and an action, applied after the
        // borrow of the hits is over.
        let mut menu: Option<(MailKey, MailMenuAction)> = None;
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
                                if let Some(action) = mail_context_menu(&drawn.row, hit.flagged) {
                                    menu = Some((
                                        MailKey::new(&hit.account_id, &hit.folder, &hit.uid),
                                        action,
                                    ));
                                }
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
        if let Some((key, action)) = menu {
            self.apply_mail_menu(key, action);
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
    pub(super) fn label_results(&mut self, ui: &mut egui::Ui) -> Option<f32> {
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
        // A label row's context menu named a mail and an action, applied after
        // the borrow of the derived view is over.
        let mut menu: Option<(MailKey, MailMenuAction)> = None;
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
                                if let Some(action) = mail_context_menu(&drawn.row, mail.flagged) {
                                    menu = Some((
                                        MailKey::new(&mail.account_id, &mail.folder, &mail.uid),
                                        action,
                                    ));
                                }
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
        if let Some((key, action)) = menu {
            self.apply_mail_menu(key, action);
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
    pub(super) fn labelled_row(
        &self,
        ui: &mut egui::Ui,
        mail: &LabelledMail,
        first: bool,
    ) -> RowResponse {
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
    pub(super) fn pump_search(&mut self, force: bool) {
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
    pub(super) fn spawn_search(&mut self) {
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
    pub(super) fn open_hit(&mut self, hit: &SearchHit) {
        self.open_mail_at(&hit.account_id, &hit.folder, &hit.uid);
    }
    /// Open one mail by where it lives: switch to the account and folder it is
    /// filed in, then open it the way a click in the list would. What a search
    /// hit's row and a label filter's row both do — neither list is a folder's,
    /// so neither row can be opened by the column's own selection.
    pub(super) fn open_mail_at(&mut self, account_id: &str, folder: &str, uid: &str) {
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
    pub(super) fn message_row(
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
    pub(super) fn thread_row(
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
    pub(super) fn search_row(
        &self,
        ui: &mut egui::Ui,
        hit: &SearchHit,
        first: bool,
    ) -> RowResponse {
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
    pub(super) fn message_card(
        &self,
        ui: &mut egui::Ui,
        card: &CardText,
        first: bool,
    ) -> RowResponse {
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
                // opens the mails under it in the list. It is a line drawing —
                // every disclosure in the app is (`docs/ui-style.md`) — and it
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
}
