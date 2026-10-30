//! The label, filter and sort popups and the flat context-menu rows shared
//! by the list and the reading pane, including the selection guard that keeps
//! label text selected while a body menu is used.

use super::*;

/// What the navigation column's label group keeps clear of the account above
/// it: the folders of an account end with a gap of their own, and this is what
/// makes the group read as its own thing under all of them.
pub(super) const LABEL_GROUP_GAP: f32 = 10.0;
/// The label popup (the label cell's menu): as wide as its checklist and its
/// new-label box want, and no taller than this before the labels scroll — a
/// user with fifty labels should not get a menu taller than the window.
pub(super) const LABEL_POPUP_WIDTH: f32 = 220.0;
/// A context menu's own width and row: one line per action, the row as wide as
/// the menu, so the whole line is the target. Smaller than the bar's choice
/// menus because a context menu is read at the pointer rather than from a cell
/// that stays on screen.
pub(super) const CONTEXT_MENU_WIDTH: f32 = 200.0;
pub(super) const CONTEXT_ROW_HEIGHT: f32 = 26.0;
pub(super) const CONTEXT_ROW_INSET: f32 = 10.0;
/// The room a menu row takes, the gap between two of them, and how far the row's
/// content stays clear of its own ends — the mark's inset and the tick's, which
/// are one number so the two line up down the menu.
pub(super) const FILTER_ROW_HEIGHT: f32 = 26.0;
pub(super) const FILTER_ROW_GAP: f32 = 1.0;
pub(super) const FILTER_ROW_INSET: f32 = 8.0;
/// The mark at a row's left, the room between it and the name, and the tick at
/// the row's end.
pub(super) const FILTER_ICON_SIZE: f32 = 16.0;
pub(super) const FILTER_ICON_GAP: f32 = 10.0;
pub(super) const FILTER_TICK_SIZE: f32 = 14.0;
/// The hover fill a row takes under the pointer, rounded like the bar's cells.
pub(super) const FILTER_ROW_CORNER_RADIUS: u8 = 4;
pub(super) const LABEL_POPUP_MAX_HEIGHT: f32 = 240.0;
pub(super) const LABEL_POPUP_FIELD_WIDTH: f32 = 120.0;
/// A message row's context menu: the title bar's mail actions under the
/// pointer that opened it, with Open first — a right-click names the row it
/// landed on, and the pane is not necessarily showing it yet. The flag's two
/// states turn over with the row, the way the bar's cell does; Delete is drawn
/// in the scheme's danger color, and the row is the whole target, not only the
/// words.
///
/// The menu is anchored to the row's own response (`Popup::context_menu`), so
/// it opens where the pointer is and closes on a click elsewhere without the
/// list having to track a second piece of state. What it asks for is returned
/// rather than acted on here: the row is drawn while the list still borrows
/// the mail, and the caller applies the action once the borrow is over.
pub(super) fn mail_context_menu(
    response: &egui::Response,
    flagged: bool,
) -> Option<MailMenuAction> {
    let mut action = None;
    egui::Popup::context_menu(response)
        .width(CONTEXT_MENU_WIDTH)
        .show(|ui| {
            ui.set_min_width(CONTEXT_MENU_WIDTH);
            ui.spacing_mut().item_spacing.y = 1.0;
            if context_menu_item(ui, "Open", true, false, false).clicked() {
                action = Some(MailMenuAction::Open);
            }
            ui.separator();
            if context_menu_item(ui, "Reply", true, false, false).clicked() {
                action = Some(MailMenuAction::Reply);
            }
            if context_menu_item(ui, "Reply all", true, false, false).clicked() {
                action = Some(MailMenuAction::ReplyAll);
            }
            if context_menu_item(ui, "Forward", true, false, false).clicked() {
                action = Some(MailMenuAction::Forward);
            }
            ui.separator();
            if context_menu_item(
                ui,
                if flagged { "Unflag" } else { "Flag" },
                true,
                false,
                false,
            )
            .clicked()
            {
                action = Some(MailMenuAction::Flag);
            }
            if context_menu_item(ui, "Export", true, false, false).clicked() {
                action = Some(MailMenuAction::Export);
            }
            if context_menu_item(ui, "Delete", true, true, false).clicked() {
                action = Some(MailMenuAction::Delete);
            }
        });
    action
}
/// One line of a context menu: flat, the window's own row height, filled under
/// the pointer with the scheme's hover — or the danger fill for a destructive
/// action, whose words turn to `on_danger` there. The whole row is the target,
/// the way a menu row's is; the menu itself supplies the frame.
pub(super) fn context_menu_item(
    ui: &mut egui::Ui,
    label: &str,
    enabled: bool,
    danger: bool,
    keep_selection: bool,
) -> egui::Response {
    let scheme = theme::of(ui.ctx());
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), CONTEXT_ROW_HEIGHT),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    if keep_selection && enabled {
        keep_label_selection(ui, &response);
    }
    if enabled && response.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(3),
            if danger {
                scheme.danger_hover
            } else {
                scheme.hover
            },
        );
    }
    let color = if !enabled {
        scheme.text_weak
    } else if danger {
        if response.hovered() {
            scheme.on_danger
        } else {
            scheme.danger
        }
    } else {
        scheme.text
    };
    let font = ui
        .style()
        .text_styles
        .get(&egui::TextStyle::Body)
        .cloned()
        .unwrap_or_else(|| egui::FontId::proportional(14.0));
    ui.painter().text(
        egui::pos2(rect.left() + CONTEXT_ROW_INSET, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        font,
        color,
    );
    if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}
/// Keep egui's label selection alive while the pointer is over a context-menu
/// row. `LabelSelectionState` clears a selection on any pointer press outside a
/// selectable label, and a popup is a layer of its own: without this, clicking
/// any action in the body's menu would drop the selected text before the menu
/// could use it (and before the user saw the action take it).
///
/// The guard is an empty, transparent "label" over the row's own response. Its
/// layer is the popup's, so the selection state treats it as a different widget
/// and never changes the range; all it does is mark the row as hovered for the
/// one check that decides whether a press clears the selection. The row sets
/// its own pointing-hand cursor afterwards.
pub(super) fn keep_label_selection(ui: &egui::Ui, response: &egui::Response) {
    let font = ui
        .style()
        .text_styles
        .get(&egui::TextStyle::Body)
        .cloned()
        .unwrap_or_else(|| egui::FontId::proportional(14.0));
    let galley = ui
        .ctx()
        .fonts_mut(|fonts| fonts.layout_no_wrap(String::new(), font, egui::Color32::TRANSPARENT));
    egui::text_selection::LabelSelectionState::label_text_selection(
        ui,
        response,
        response.rect.min,
        galley,
        egui::Color32::TRANSPARENT,
        egui::Stroke::NONE,
    );
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
pub(super) fn menu_row(
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
pub(super) fn menu_heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(FILTER_ROW_GAP * 2.0);
    ui.label(egui::RichText::new(text).small().weak());
    ui.add_space(FILTER_ROW_GAP);
}

impl KimoApp {
    /// The label popup: the mail's labels as a checklist, and a box that makes a
    /// new one by putting it on the mail — which is the only way a label comes
    /// into being. Anchored to the cell that opened it (`Popup::menu`), and
    /// closed by a click outside it rather than by a click on a label: picking
    /// several is what the popup is for.
    pub(super) fn label_popup(&mut self, response: &egui::Response, key: &MailKey) {
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
    pub(super) fn filter_popup(&mut self, response: &egui::Response) {
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
    pub(super) fn set_list_filters(&mut self, filters: ListFilters) {
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
    pub(super) fn sort_popup(&mut self, response: &egui::Response) {
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
    pub(super) fn set_list_sort(&mut self, sort: ListSort) {
        if self.list_sort == sort {
            return;
        }
        self.list_sort = sort;
        self.ctx.request_repaint();
    }
}
