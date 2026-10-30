//! The reading pane: its cards, headers, body and attachments; the Blitz
//! view; body context menus; and the per-mail render/body caches.

use super::*;

/// A fingerprint of a body. Length alone would not do: a re-fetch can bring a
/// different body of the same size under a uid the pane already shows.
pub(super) fn body_fingerprint(html: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    html.hash(&mut hasher);
    hasher.finish()
}
/// The popup pane's back button: a round, chevron-only cell floating over
/// the mail (see `mail_content`) — its diameter, how far it keeps from the
/// window's edge under the title bar, and the chevron it carries with the
/// stroke that chevron draws with.
pub(super) const POPUP_BACK_DIAMETER: f32 = 36.0;
pub(super) const POPUP_BACK_MARGIN: f32 = 10.0;
pub(super) const POPUP_BACK_CHEVRON: f32 = 12.0;
pub(super) const POPUP_BACK_STROKE: f32 = 1.5;
/// Decode a stored inline image into an egui texture. Runs on the UI thread
/// but only once per message/cid; inline mail images are small.
pub(super) fn decode_inline_image(
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
pub(super) fn sender_color(scheme: ColorScheme, from: &str) -> egui::Color32 {
    let hash = from
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    scheme.avatar[(hash as usize) % scheme.avatar.len()]
}
/// First character of the display name (or e-mail local part), for avatars.
pub(super) fn sender_initial(from: &str) -> String {
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
/// `action` is the last line of the card's tooltip: what a click on it would do,
/// which is not the same in the two places the card is drawn — in the reading
/// pane it fetches the file, and in the compose card it takes the file off the
/// mail. `removable` draws the cross a file that has not been sent yet can be
/// taken off with, and reserves the room for it: a mail's own attachment is not
/// Kimo's to take away, so the reading pane's cards have no cross at all.
pub(super) fn attachment_card(
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
pub(super) fn attachment_meta(attachment: &Attachment) -> String {
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
pub(super) fn human_size(bytes: u64) -> String {
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
/// One row of the folder list: kind icon, name, and the unread count on the
/// right. The whole row is the click target and carries the selection
/// highlight — a `selectable_label` would leave the icon outside its own
/// highlight and put the count inside the name. The colors come straight from
/// the scheme instead of from egui's widget states, which is what lets an
/// unread folder read brighter than a read one.
/// Account header in the navigation column: a disclosure chevron
/// (`paint_chevron`, down while the account's folders are out and right while
/// they are away), the account name and its unread total. Clicking the row
/// collapses or expands the account's folders.
/// The labels a mail carries, drawn as one run of chips on the mail's own card:
/// what the user marked it with, said on the mail rather than only in the popup
/// that put it there. Each chip is as wide as its name wants, and the run wraps
/// where the next one does not fit — the way the attachment cards under the body
/// do — so a mail with a handful of labels says so without widening the card.
pub(super) fn label_chips(ui: &mut egui::Ui, scheme: ColorScheme, labels: &BTreeSet<String>) {
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

impl KimoApp {
    /// The reading pane — the flexible right column in the column layout, the
    /// area over the list in the popup one: the selected mail on the card the
    /// pane floats on, the lighter content surface inset in the chrome by
    /// CONTENT_GUTTER. With nothing selected there is no card at all: the pane
    /// is bare chrome and the placeholder sits directly on it.
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
    /// The body context menu: what a right-click on a mail's text, link or
    /// image asked for. Drawn at the top of the pane's frame, before the body
    /// is laid out again, so the copy event it queues is seen by the labels
    /// drawn later in the same pass. Its rows also keep egui's label selection
    /// alive across their own press (`keep_label_selection`), so the selected
    /// text stays highlighted while the menu is used. The menu is a
    /// `PopupKind::Menu` at the pointer, so it sits above the mail it came
    /// from and closes on an outside click or Escape like the bar's menus.
    pub(super) fn body_context_menu(&mut self, ui: &egui::Ui, conversation: &Conversation) {
        let Some(menu) = self.body_menu.take() else {
            return;
        };
        let same_mail = self.selected_account.as_deref() == Some(menu.key.account.as_str())
            && self.selected_folder.as_deref() == Some(menu.key.folder.as_str())
            && conversation
                .messages
                .iter()
                .any(|mail| mail.uid == menu.key.uid);
        if !same_mail {
            self.body_menu = None;
            self.body_menu_open = false;
            return;
        }
        let id = egui::Id::new((
            "mail-body-menu",
            menu.key.account.as_str(),
            menu.key.folder.as_str(),
            menu.key.uid.as_str(),
        ));
        let mut open = self.body_menu_open;
        let mut action: Option<BodyMenuAction> = None;
        let has_selection = ui
            .ctx()
            .with_plugin::<egui::text_selection::LabelSelectionState, _>(|selection| {
                selection.has_selection()
            })
            .unwrap_or(false);
        let remote = |url: &str| url.starts_with("http://") || url.starts_with("https://");
        egui::Popup::new(
            id,
            ui.ctx().clone(),
            egui::PopupAnchor::Position(menu.pos),
            egui::LayerId::new(egui::Order::Foreground, id),
        )
        .open_bool(&mut open)
        .kind(egui::PopupKind::Menu)
        .layout(egui::Layout::top_down_justified(egui::Align::Min))
        .width(CONTEXT_MENU_WIDTH)
        .show(|ui| {
            ui.set_min_width(CONTEXT_MENU_WIDTH);
            ui.spacing_mut().item_spacing.y = 1.0;
            let mut separated = false;
            match &menu.target {
                crate::html::BodyContext::Link(url) => {
                    if context_menu_item(ui, "Open link", true, false, true).clicked() {
                        action = Some(BodyMenuAction::Open(url.clone()));
                    }
                    if context_menu_item(ui, "Copy link address", true, false, true).clicked() {
                        action = Some(BodyMenuAction::Copy(url.clone()));
                    }
                    separated = true;
                }
                crate::html::BodyContext::Image(src) if remote(src) => {
                    if context_menu_item(ui, "Open image", true, false, true).clicked() {
                        action = Some(BodyMenuAction::Open(src.clone()));
                    }
                    if context_menu_item(ui, "Copy image address", true, false, true).clicked() {
                        action = Some(BodyMenuAction::Copy(src.clone()));
                    }
                    separated = true;
                }
                crate::html::BodyContext::Image(_) | crate::html::BodyContext::Text => {}
            }
            if separated {
                ui.separator();
            }
            // The guard on every row is what keeps the selection alive
            // through the press, so Copy can be an ordinary click and the
            // selected text stays visible after it.
            let copy = context_menu_item(ui, "Copy", has_selection, false, true);
            if copy.clicked() {
                action = Some(BodyMenuAction::CopySelection);
            }
            if let Some(text) = &menu.text {
                if context_menu_item(ui, "Copy message text", true, false, true).clicked() {
                    action = Some(BodyMenuAction::Copy(text.clone()));
                }
            }
        });
        self.body_menu_open = open;
        if open {
            self.body_menu = Some(menu);
        }
        if let Some(action) = action {
            match action {
                BodyMenuAction::CopySelection => {
                    // The selection is egui's; this event is what makes it
                    // copy. The labels have not been drawn yet this frame, so
                    // they will see it before their end-of-pass clears the
                    // selection.
                    ui.ctx()
                        .input_mut(|input| input.events.push(egui::Event::Copy));
                }
                BodyMenuAction::Copy(text) => ui.ctx().copy_text(text),
                BodyMenuAction::Open(url) => crate::html::open_url(ui, &url),
            }
        }
    }
    pub(super) fn mail_content(&mut self, ui: &mut egui::Ui) {
        let scheme = theme::of(ui.ctx());
        // The chrome is painted here rather than by the central panel's frame:
        // a frame fills its *content's* rect, which the card is inset from, so
        // the pane's right edge would be left unpainted.
        let panel = ui.max_rect();
        // Claim the whole column: the reading pane is the flexible one, and its
        // rect is whatever the two side panels leave. The card is inset from
        // that, so without this the pane would paint only what it drew.
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
            // Nothing on the card, so nothing to keep parsed behind it, and no
            // body to right-click either.
            self.body.forget();
            self.body_menu = None;
            self.body_menu_open = false;
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

        // A body right-click from an earlier frame, before the scroll area
        // draws the body again: Copy has to reach the label selection before
        // the click that asked for it clears it.
        if self.body_menu.is_some() {
            self.body_context_menu(ui, &conversation);
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
    pub(super) fn compose_inline_card(
        &mut self,
        ui: &mut egui::Ui,
    ) -> (egui::layers::ShapeIdx, egui::Rect) {
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
    pub(super) fn popup_back(&mut self, ui: &egui::Ui, panel: egui::Rect) {
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
                self.body_menu = None;
                self.body_menu_open = false;
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
    pub(super) fn mail_card(
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
    pub(super) fn mail_sheet(&mut self, ui: &mut egui::Ui, msg: &MailMessage) {
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
    pub(super) fn attachment_cards(&mut self, ui: &mut egui::Ui, msg: &MailMessage, column: f32) {
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
    pub(super) fn download_attachment(&mut self, uid: &str, ordinal: usize, name: &str) {
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
    pub(super) fn mail_body(&mut self, ui: &mut egui::Ui, msg: &MailMessage) {
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
                context: None,
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
            // A right-click on the body is the pane's menu to show. It is kept
            // here with the mail it belongs to, because the menu's actions —
            // copy the text, open the link, copy the image address — are about
            // this mail, not the one the pane might show next.
            if let Some(request) = html_ctx.context.take() {
                self.set_body_menu(msg, request.target, request.pos);
            }
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
            let response = ui.label(msg.body_text.as_str());
            if response.secondary_clicked() {
                let pos = response
                    .interact_pointer_pos()
                    .or_else(|| ui.ctx().pointer_hover_pos())
                    .unwrap_or(response.rect.center());
                self.set_body_menu(msg, crate::html::BodyContext::Text, pos);
            }
        } else {
            self.body.forget();
            ui.label(egui::RichText::new("(no text body)").weak());
        }
    }
    /// Remember a right-click in one mail's body for the menu the pane draws
    /// on the next frame. The text is taken here because it is the mail's own,
    /// and the HTML fallback is only needed for a mail without a text part.
    pub(super) fn set_body_menu(
        &mut self,
        msg: &MailMessage,
        target: crate::html::BodyContext,
        pos: egui::Pos2,
    ) {
        let Some(key) = self.pane_mail_key(&msg.uid) else {
            return;
        };
        let text = if !msg.body_text.is_empty() {
            Some(msg.body_text.clone())
        } else {
            let text = crate::mail::html_to_plain(msg.body_html.as_deref().unwrap_or(""));
            (!text.is_empty()).then_some(text)
        };
        self.body_menu = Some(BodyMenu {
            pos,
            target,
            key,
            text,
        });
        self.body_menu_open = true;
    }
}
