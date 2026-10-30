//! The navigation column: account sections, folder and label rows, and the
//! per-account sync cell and its hover state.

use super::*;

/// The account row's disclosure chevron (`paint_chevron`), the size of a thread
/// row's: with it the app draws no solid triangle anywhere (`docs/ui-style.md`).
pub(super) const ACCOUNT_CHEVRON: f32 = 10.0;
/// The sync cell at the end of an account row: the title bar's sync glyph at
/// the row's scale, and the spinner it becomes while that one account is
/// syncing. The cell is a real target on every account row, not a hover-only
/// mark, because it is the one control besides a folder click that reaches a
/// single account.
pub(super) const ACCOUNT_SYNC_SIZE: f32 = 20.0;
pub(super) const ACCOUNT_SYNC_SPINNER: f32 = 14.0;
pub(super) fn account_row(
    ui: &mut egui::Ui,
    scheme: ColorScheme,
    account_id: &str,
    name: &str,
    state: AccountRowState,
) -> AccountRowResponse {
    const ROW_HEIGHT: f32 = 24.0;

    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_HEIGHT),
        egui::Sense::click(),
    );
    let sync_rect = egui::Rect::from_center_size(
        egui::pos2(
            rect.right() - 4.0 - ACCOUNT_SYNC_SIZE / 2.0,
            rect.center().y,
        ),
        egui::Vec2::splat(ACCOUNT_SYNC_SIZE),
    );
    // The account's own sync cell, registered after the row so it is the
    // topmost of the two: a click on it is not a click that folds the account.
    // It is a target on every row, drawn or not, like the other cells the app
    // offers at an edge.
    let sync = ui.interact(
        sync_rect,
        egui::Id::new(("account-sync", account_id)),
        egui::Sense::click(),
    );
    if !ui.is_rect_visible(rect) {
        return AccountRowResponse {
            row: response,
            sync,
        };
    }
    let hovered = response.hovered() || sync.hovered();
    if hovered {
        ui.painter().rect_filled(rect, 3.0_f32, scheme.hover);
    }

    // The disclosure (`paint_chevron`): right while collapsed, down while open —
    // the app's one disclosure shape (`docs/ui-style.md`), which is what the
    // navigation column's last solid triangle became.
    let center = egui::pos2(rect.left() + 10.0, rect.center().y);
    paint_chevron(
        ui.painter(),
        center,
        ACCOUNT_CHEVRON,
        !state.collapsed,
        scheme.text_weak,
    );

    let left = center.x + 10.0;
    let mut right = sync_rect.left() - 4.0;
    if state.unread > 0 {
        let small = egui::TextStyle::Small.resolve(ui.style());
        let count = ui.ctx().fonts_mut(|fonts| {
            fonts.layout_no_wrap(state.unread.to_string(), small, scheme.text_weak)
        });
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
    let label = truncate_galley(
        ui.ctx(),
        name,
        body.size,
        scheme.text,
        (right - left).max(1.0),
    );
    ui.painter().galley(
        egui::pos2(left, rect.center().y - label.size().y / 2.0),
        label,
        scheme.text,
    );

    // The cell itself: the bar's sync glyph while the account is idle, a
    // spinner while its own sync is running, and a failure dot when the last
    // one did not finish. The row's hover fill is already behind it; the
    // glyph takes the full text colour under the pointer so the cell reads as
    // a control rather than as decoration.
    if state.syncing {
        ui.put(
            sync_rect,
            egui::Spinner::new()
                .color(scheme.accent)
                .size(ACCOUNT_SYNC_SPINNER),
        );
    } else if hovered {
        // The cell is a hover control: the row keeps its room whether or not
        // it is shown, so the unread count and the name do not move under the
        // pointer.
        let color = if sync.hovered() {
            scheme.text
        } else {
            scheme.text_weak
        };
        paint_sync_icon(ui.painter(), sync_rect, color);
    }
    if state.failed {
        ui.painter().circle_filled(
            egui::pos2(sync_rect.right() - 3.0, sync_rect.top() + 3.0),
            SYNC_BADGE_RADIUS,
            scheme.danger,
        );
    }
    let sync = sync
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(if state.syncing {
            format!("Syncing {name}…")
        } else if state.failed {
            format!("Sync {name} — the last sync failed")
        } else {
            format!("Sync {name} only")
        });
    AccountRowResponse {
        row: response,
        sync,
    }
}
/// One folder in the navigation column. `listed` is whether the server still
/// has it: a row kept for the mail cached under a folder it no longer lists is
/// drawn weak whatever its unread count, and says so on hover — it is here for
/// the mail, not for the mailbox. It stays clickable, because its mail is the
/// only copy of it on this machine.
pub(super) fn folder_row(
    ui: &mut egui::Ui,
    scheme: ColorScheme,
    icon: Option<RowIcon<'_>>,
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
        match icon {
            // The tag is drawn, not baked: it is the same glyph the label
            // group and the title bar's label button use, at the row's size.
            RowIcon::Label => paint_label_icon(ui.painter(), icon_rect, color, ICON_SIZE - 2.0),
            // The icon is white on transparency, so this tint is its color.
            RowIcon::Texture(icon) => {
                ui.painter().image(
                    icon.id(),
                    icon_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    color,
                );
            }
        }
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
pub(super) fn label_group_header(ui: &mut egui::Ui, scheme: ColorScheme) {
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
pub(super) fn label_row(
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

impl KimoApp {
    /// Left column: every account as a collapsible section of its folders.
    /// The selection can be a folder of any of them — switching accounts is
    /// picking a folder here, so the title bar needs no account menu.
    pub(super) fn nav(&mut self, ui: &mut egui::Ui) {
        let scheme = theme::of(ui.ctx());
        // Folders are indented under their account, but egui's `indent` also
        // marks the indented block with a faint guide line down its left,
        // which in this column reads as a rule drawn through the folder list.
        // The HTML renderer's lists, the only other users of `indent`, keep
        // theirs.
        ui.visuals_mut().indent_has_left_vline = false;

        if self.accounts.is_empty() {
            // Nothing to scroll here, so the bar keeps its chrome band over
            // this column and the placeholder sits below it.
            ui.add_space(TITLEBAR_HEIGHT);
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
        let output = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // The title bar floats over this column too: its height is
                // space in the scroll area, not padding above it, so the
                // folders pass under the glass the way the list's rows do.
                ui.add_space(TITLEBAR_HEIGHT + 2.0);
                for (id, name, protocol) in &accounts {
                    self.account_section(ui, scheme, id, name, *protocol);
                }
                // Always the column's last group: the labels belong to no
                // account, so they cannot be filed under one.
                self.label_group(ui, scheme);
            });
        self.nav_scroll = output.state.offset.y;
    }
    /// The navigation column's last group: the user's labels, under every
    /// account and its folders, drawn like folders because a label is a
    /// first-class filter beside them. A selected one carries the selection
    /// fill, and the message list shows the mail that carries **all** of the
    /// selected ones (`KimoApp::toggle_label`).
    pub(super) fn label_group(&mut self, ui: &mut egui::Ui, scheme: ColorScheme) {
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
    pub(super) fn account_section(
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
        let (unread, placeholder, syncing, failed, error) = {
            let state = self.states.entry(account_id.to_string()).or_default();
            let nav = state.nav();
            let (unread, placeholder) = (nav.unread, nav.placeholder);
            (
                unread,
                placeholder,
                state.syncing,
                state.error.is_some(),
                state.error.clone(),
            )
        };
        let drawn = account_row(
            ui,
            scheme,
            account_id,
            account_name,
            AccountRowState {
                unread,
                collapsed,
                syncing,
                failed,
            },
        );
        if drawn.sync.clicked() {
            // A click on the cell is the account's own sync, not the section's
            // fold: the cell is registered over the row, and this is the one
            // place the two are told apart.
            self.sync_account(account_id);
        } else if drawn.row.clicked() {
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
                    // mailboxes keep one glyph, and a label keeps the tag.
                    let open = !row.label && is_selected && row.kind == FolderKind::Folder;
                    let icon = if row.label {
                        Some(RowIcon::Label)
                    } else {
                        icons.get(&(row.kind, open)).map(RowIcon::Texture)
                    };
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
}
