//! The search popup and its clock: the bar, query controls, settle/spawn
//! path, and the result count.

use super::*;

/// How long the search popup waits after the last keystroke before asking the
/// database: long enough to swallow a burst of typing, short enough to feel
/// immediate. Enter skips the wait.
pub(super) const SEARCH_SETTLE: Duration = Duration::from_millis(250);
/// The search box's own margins, *inside* that width: the magnifier's inset on
/// the left, and a hair of room on the right so the query does not touch the
/// box's edge.
pub(super) const SEARCH_BOX_MARGIN: egui::Margin = egui::Margin {
    left: SEARCH_ICON_INSET,
    ..egui::Margin::symmetric(4, 2)
};
/// Corner radius of that box. egui's stock widget radius is 2, which is right
/// for a button but leaves this box looking like a rectangle; a rounder corner
/// makes it read as the one field in the bar.
pub(super) const SEARCH_BOX_CORNER_RADIUS: u8 = 8;
/// Room the magnifier takes inside the search box, and the glyph's size. The
/// inset is the field's left margin, so the text begins after the icon.
pub(super) const SEARCH_ICON_INSET: i8 = 22;
pub(super) const SEARCH_ICON_SIZE: f32 = 15.0;
/// The search popup's own bar: taller than a form's field, so the magnifier and
/// the clear cross have room inside it, with an inset at each end taken out of
/// the text's own margins.
pub(super) const SEARCH_BAR_HEIGHT: f32 = 28.0;
pub(super) const SEARCH_BAR_MARGIN: egui::Margin = egui::Margin {
    left: SEARCH_ICON_INSET,
    right: SEARCH_ICON_INSET,
    top: 4,
    bottom: 4,
};
/// The round cross that empties the bar, the size of a file card's own cross
/// (`ATTACHMENT_CLOSE_SIZE`) and round for the same reason: the button is a
/// circle, and its radius is half of it.
pub(super) const SEARCH_CLEAR_SIZE: f32 = 18.0;
pub(super) const SEARCH_CLEAR_RADIUS: u8 = 9;
/// True when a popup field was left by pressing Enter — the search popup takes
/// that as "run it now" from any of its boxes, not only the text one.
pub(super) fn entered(response: &egui::Response, ui: &egui::Ui) -> bool {
    response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
}
/// One label + box row of the search popup's filter grid. The box is quiet —
/// the fill and no line until the pointer or the keyboard is on it — the way
/// the compose card's address rows are (`docs/ui-style.md`). Returns true when
/// the box was left by pressing Enter.
pub(super) fn filter_row(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) -> bool {
    ui.label(label);
    let response = ui.add(
        Field::new(
            egui::TextEdit::singleline(value)
                .hint_text(hint)
                .desired_width(300.0),
        )
        .quiet(),
    );
    let apply = entered(&response, ui);
    ui.end_row();
    apply
}
/// "1 result" / "12 results" / "200+ results": at `SEARCH_LIMIT` the database
/// stopped asking, so the count is a floor rather than a total.
pub(super) fn results_label(count: usize) -> String {
    if count >= SEARCH_LIMIT {
        format!("{count}+ results")
    } else if count == 1 {
        "1 result".to_string()
    } else {
        format!("{count} results")
    }
}
/// The search popup settles the way the Blitz pane does: the database is queried
/// once the user stops typing. `pending` is the query the clock is running for,
/// and when it started.
pub(super) fn search_settle(
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

impl KimoApp {
    /// Open the search popup with the keyboard already in its text field.
    pub(super) fn open_search(&mut self) {
        self.show_search = true;
        self.search_focus = true;
    }
    /// The search popup, opened from the title-bar magnifier or Ctrl+F: the
    /// free text, the scope and every filter, which used to be a bar above the
    /// message list. Searching stays live while it is open — the count under
    /// the fields follows the query — and Enter (or Search) applies it at once
    /// and closes, leaving the results on screen. Returns true when the query
    /// should run without waiting for the settle clock.
    pub(super) fn search_modal(&mut self, ctx: &egui::Context) -> bool {
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
            // The bar the query is typed in, and the popup's first row: no
            // heading over it and no close button beside it — the bar is what
            // the popup is, Escape and a click outside close it, and the typing
            // goes into it the moment it opens. Kimo draws the box (`Field`),
            // the magnifier inside its left edge (`paint_search_icon`, the
            // title bar's own drawing) and, once there is something to clear,
            // the round cross inside its right (`paint_close_icon`).
            let text = ui.add(
                Field::new(
                    egui::TextEdit::singleline(&mut self.search.text)
                        .hint_text("Search all mail…")
                        .desired_width(f32::INFINITY)
                        .min_size(egui::vec2(0.0, SEARCH_BAR_HEIGHT))
                        .vertical_align(egui::Align::Center),
                )
                .margin(SEARCH_BAR_MARGIN),
            );
            let scheme = theme::of(ui.ctx());
            let bar = text.rect;
            let weak = ui.visuals().weak_text_color();
            paint_search_icon(
                ui.painter(),
                egui::Rect::from_center_size(
                    egui::pos2(bar.left() + SEARCH_ICON_INSET as f32 / 2.0, bar.center().y),
                    egui::Vec2::splat(SEARCH_ICON_SIZE),
                ),
                weak,
            );
            if !self.search.text.is_empty() {
                // The cross empties the bar's own text and not the filters: it
                // sits in the bar, and each filter has a box of its own to
                // empty. The list behind follows at once (`run_now`) rather
                // than waiting out the settle clock.
                let cross = egui::Rect::from_center_size(
                    egui::pos2(bar.right() - SEARCH_ICON_INSET as f32 / 2.0, bar.center().y),
                    egui::Vec2::splat(SEARCH_CLEAR_SIZE),
                );
                let clear = ui.interact(cross, text.id.with("clear"), egui::Sense::click());
                if clear.hovered() {
                    ui.painter()
                        .rect_filled(cross, SEARCH_CLEAR_RADIUS, scheme.hover);
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                paint_close_icon(
                    ui.painter(),
                    cross,
                    if clear.hovered() { scheme.text } else { weak },
                );
                if clear.clicked() {
                    self.search.text.clear();
                    self.search_focus = true;
                    run_now = true;
                }
                clear.on_hover_text("Clear the search");
            }
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
                // The combo is the card's own kind of control, not a form's box:
                // no fill and no line at rest, the hover kept, and the app's
                // chevron where egui would put its filled triangle — the same
                // treatment the compose card's From list gets, and the reason
                // the pointer's own state is kept for the chevron painted after
                // the button (`flatten_widgets`, `paint_combo_chevron`).
                flatten_widgets(ui, theme::of(ui.ctx()).hover);
                let icon = std::rc::Rc::new(std::cell::Cell::new((egui::Rect::NOTHING, false)));
                let hook = std::rc::Rc::clone(&icon);
                let list = egui::ComboBox::from_id_salt("search-scope")
                    .selected_text(label)
                    .width(170.0)
                    .icon(move |_, rect, _, is_open| {
                        hook.set((rect, is_open));
                    })
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
                let (place, open) = icon.get();
                paint_combo_chevron(ui, &list.response, place, open);
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

            // What the query found, when there is a query — and nothing at all
            // otherwise, rather than a rule with an empty row under it. No
            // Search button stands here either: the query runs as it is typed
            // (`SEARCH_SETTLE`), and Enter only skips that wait.
            if self.search_running || self.search.is_active() {
                ui.add_space(6.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if self.search_running {
                        ui.spinner();
                        ui.label(egui::RichText::new("Searching…").small().weak());
                    } else {
                        ui.label(
                            egui::RichText::new(results_label(self.search_hits.len()))
                                .small()
                                .weak(),
                        );
                    }
                });
            }
        });
        // Backdrop click or Escape closes the popup. What was typed stays
        // applied: the search is live, so closing is not a cancel.
        if modal.should_close() {
            open = false;
        }
        self.show_search = open;
        run_now
    }
}
