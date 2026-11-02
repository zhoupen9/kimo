//! The Preferences modal and its pages: accounts and the add/edit form,
//! plugins, general sync settings, appearance (theme/fonts/renderer), graphics
//! and About.

use super::*;

/// The icon Preferences → About draws, and the gap between it and the app's
/// name.
pub(super) const ABOUT_ICON_SIZE: f32 = 72.0;
pub(super) const ABOUT_ICON_GAP: f32 = 16.0;
/// Preferences → Appearance: the palette in force, so every slot the app draws
/// with can be read at a glance — the theme's own values with the chosen
/// accent among them. Read-only: it documents what the app draws with, and the
/// two slots that *are* configurable are picked above it (the theme's radios
/// and the accent's chips).
pub(super) fn color_scheme_preview(ui: &mut egui::Ui, scheme: ColorScheme) {
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
pub(super) const ACCENT_CHIP_SIZE: f32 = 20.0;
pub(super) const ACCENT_CHIP_CORNER_RADIUS: u8 = 5;
pub(super) const ACCENT_CHIP_GAP: f32 = 10.0;
pub(super) const ACCENT_CHIP_RING: f32 = 2.0;
pub(super) const ACCENT_CHIP_RING_GAP: f32 = 3.0;
/// One accent chip in Preferences → Appearance: the colour itself, ringed in
/// the app's own ink while it is the one in force — a ring, because the chip
/// *is* a colour and a mark drawn on it would be a second colour to read — and
/// named on hover, which is what tells two neighbouring blues apart.
///
/// `dark` picks the theme's variant of the accent, so the chips show the
/// colours the app is drawing with rather than the other scheme's.
pub(super) fn accent_chip(
    ui: &mut egui::Ui,
    dark: bool,
    accent: Accent,
    chosen: bool,
) -> egui::Response {
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
pub(super) fn swatch(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 2.0, color);
    ui.painter().rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color),
        egui::StrokeKind::Inside,
    );
}
/// The cross that closes the preferences modal: a round target at the end of
/// the heading row, the search bar's clear cross one size up. A dialog's
/// heading has room a bar's inset does not, and the target has to read as a
/// button rather than as part of the title.
pub(super) const PREFS_CLOSE_SIZE: f32 = 22.0;
pub(super) const PREFS_CLOSE_RADIUS: u8 = 11;
/// A Preferences font picker: the unset entry (`default_label`) plus every
/// installed family, each entry drawn in its own shape so fixed-width families
/// stand out. Returns true when the selection changed.
pub(super) fn font_picker(
    ui: &mut egui::Ui,
    id: &str,
    families: &Option<Vec<FontFamily>>,
    current: &mut Option<String>,
    default_label: &str,
) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_salt(id)
        .selected_text(current.clone().unwrap_or_else(|| default_label.to_string()))
        .width(240.0)
        .height(320.0)
        .show_ui(ui, |ui| {
            ui.set_min_width(260.0);
            if ui
                .selectable_label(current.is_none(), default_label)
                .clicked()
            {
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

impl KimoApp {
    /// Save the add/edit form: mint an account (and a fresh id) for a new
    /// one, or replace the edited account in place, then sync it so the
    /// window shows what the credentials actually reach.
    pub(super) fn save_account_form(&mut self) {
        self.save_account_form_impl(false);
    }
    /// Save the form. `sign_in` is the Sign-in button's path: the account
    /// row is written the same way, but the job is the browser flow and the
    /// form stays open on the account, waiting with the user — a sign-in
    /// whose window closed behind it looks exactly like one that never
    /// started.
    pub(super) fn save_account_form_impl(&mut self, sign_in: bool) {
        self.account_error = None;
        let form = self.account_form.clone();
        let edited = form
            .editing
            .clone()
            .and_then(|id| self.accounts.iter().position(|a| a.id == id));
        let previous = edited.map(|index| self.accounts[index].clone());
        let account = match &previous {
            Some(previous) => form.to_account(previous.id.clone(), Some(previous)),
            None => form.to_account(self.unique_account_id(&slug(&form.name)), None),
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
        // The sign-in path keeps the form open on the account it saved — a
        // fresh form's `editing` becomes the row it just minted, so the
        // answer that comes back finds the form it belongs to.
        if sign_in && self.account_form.editing.is_none() {
            self.account_form.editing = Some(account_id.clone());
        }

        if switches_mailbox {
            // What is stored describes the mailbox the old settings reached,
            // and the new ones may be a different server entirely, so the job
            // below purges it before fetching rather than show mail from an
            // account that is gone. Its in-flight renders go too.
            self.states
                .insert(account_id.clone(), AccountUiState::default());
            self.forget_blitz_pages(&account_id);
        }

        // One job saves the account row and then does the one thing that is
        // next, so no second job can reorder those steps. The Sign-in
        // button's job is the browser flow; a plain Save on a plugin account
        // without tokens cannot sync, so its first sync *is* the sign-in too
        // — but through this one spawn, never a second.
        if sign_in || (account.is_plugin() && account.access_token.is_none()) {
            self.plugin_signing_in = Some(account.id.clone());
            sync::spawn(
                Job::PluginSignIn {
                    account,
                    purge: switches_mailbox,
                },
                self.ctx.clone(),
                self.tx.clone(),
            );
        } else {
            sync::spawn(
                Job::Sync {
                    account,
                    purge: switches_mailbox,
                    folder: None,
                    save: true,
                },
                self.ctx.clone(),
                self.tx.clone(),
            );
        }

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
        if !sign_in {
            self.show_account_form = false;
        }
    }
    /// Save the form and send the account straight to the browser sign-in.
    /// The row has to exist first — the tokens land in it — and the save
    /// spawns the one job; a second spawn here would be a second browser.
    pub(super) fn start_plugin_sign_in(&mut self) {
        if !self.account_form.is_valid() {
            return;
        }
        self.save_account_form_impl(true);
    }
    /// An id for a new account: the slug of its name, numbered if that is
    /// taken. Ids name the cache files, so they are never reused.
    pub(super) fn unique_account_id(&self, base: &str) -> String {
        let mut id = base.to_string();
        let mut n = 1;
        while self.accounts.iter().any(|a| a.id == id) {
            n += 1;
            id = format!("{base}-{n}");
        }
        id
    }
    /// The preferences modal, opened from the title-bar cog: a sidebar of
    /// pages with the account configuration (previously split between a
    /// standalone window and the account menu) and the appearance switches
    /// (previously title-bar buttons).
    pub(super) fn preferences_modal(&mut self, ctx: &egui::Context) {
        let mut open = self.show_preferences;
        let modal = egui::Modal::new(egui::Id::new("preferences")).show(ctx, |ui| {
            ui.set_width(600.0);
            ui.horizontal(|ui| {
                ui.heading("Preferences");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // A cross rather than a worded button: the modal's own
                    // chrome, drawn like the search bar's clear cross one size
                    // up, and it says what it does on hover. Escape and the
                    // backdrop close the modal too.
                    let (rect, _) = ui.allocate_exact_size(
                        egui::Vec2::splat(PREFS_CLOSE_SIZE),
                        egui::Sense::hover(),
                    );
                    let response = ui.interact(
                        rect,
                        egui::Id::new("preferences-close"),
                        egui::Sense::click(),
                    );
                    let scheme = theme::of(ui.ctx());
                    let hovered = response.hovered();
                    if hovered {
                        ui.painter()
                            .rect_filled(rect, PREFS_CLOSE_RADIUS, scheme.hover);
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    paint_close_icon(
                        ui.painter(),
                        rect,
                        if hovered {
                            scheme.text
                        } else {
                            scheme.text_weak
                        },
                    );
                    if response.clicked() {
                        open = false;
                    }
                    response.on_hover_text("Close preferences");
                });
            });
            ui.separator();
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(150.0);
                    for (tab, label) in [
                        (PrefsTab::General, "General"),
                        (PrefsTab::Accounts, "Accounts"),
                        (PrefsTab::Plugins, "Plugins"),
                        (PrefsTab::Appearance, "Appearance"),
                        (PrefsTab::Graphics, "Graphics"),
                        (PrefsTab::About, "About"),
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
                        PrefsTab::Plugins => self.plugins_page(ui),
                        PrefsTab::Appearance => self.appearance_page(ui),
                        PrefsTab::Graphics => self.graphics_page(ui),
                        PrefsTab::About => self.about_page(ui),
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
    pub(super) fn accounts_page(&mut self, ui: &mut egui::Ui) {
        if self.show_account_form {
            self.account_form_page(ui);
            return;
        }

        let mut switch_to: Option<Account> = None;
        let mut edit: Option<String> = None;
        let mut remove: Option<Account> = None;

        // A removal that failed left the account (and its row) in place; this
        // is the line that says why it came back.
        if let Some(error) = &self.account_error {
            ui.label(egui::RichText::new(error).color(ui.visuals().error_fg_color));
            ui.add_space(4.0);
        }

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
                                remove = Some(account.clone());
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

        // Which provider presets the form offers. The standard protocols are
        // not a preset: they have no entry here and cannot be switched off.
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Provider presets").strong());
        ui.label(
            egui::RichText::new(
                "A preset fills in a provider's servers and sign-in notes in the account \
                 form. Plain IMAP, POP3 and SMTP servers are always available.",
            )
            .small()
            .weak(),
        );
        for key in VendorKey::ALL
            .iter()
            .copied()
            .filter(|key| *key != VendorKey::Custom)
        {
            let mut on = self.vendor_enabled(key);
            if ui.checkbox(&mut on, key.label()).changed() {
                if on {
                    self.disabled_vendors.remove(&key);
                } else {
                    self.disabled_vendors.insert(key);
                }
                self.persist_settings();
            }
        }

        if let Some(account) = switch_to {
            self.select_folder(account.id.clone(), "INBOX".to_string(), account.protocol);
        }
        if let Some(id) = edit {
            self.open_edit_account_form(&id);
        }
        if let Some(account) = remove {
            self.delete_account(account);
            self.confirm_delete = None;
        }
    }
    /// Open the account form on a configured account, pre-filled, so the
    /// dialog edits it instead of adding another one.
    pub(super) fn open_edit_account_form(&mut self, account_id: &str) {
        let Some(account) = self.accounts.iter().find(|a| a.id == account_id) else {
            return;
        };
        self.account_form = AccountForm::edit(account);
        // A preset switched off in Preferences is not a choice the form offers
        // any more; the account edits as the plain server it still is.
        if !self.vendor_enabled(self.account_form.vendor) {
            self.account_form.vendor = VendorKey::Custom;
        }
        self.show_account_form = true;
        self.confirm_delete = None;
    }
    /// Preferences → Accounts → form: fill in a new account, or edit the
    /// one the form was opened on. The standalone "Add account" window used
    /// to show this.
    pub(super) fn account_form_page(&mut self, ui: &mut egui::Ui) {
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

        // A preset that was switched off since this form opened is no longer
        // offered: fall back to Custom rather than keep a vendor the picker
        // does not list.
        if !self.vendor_enabled(self.account_form.vendor) {
            self.account_form.vendor = VendorKey::Custom;
        }
        // The plugin providers installed: the dedicated entries. A vendor
        // whose plugin is here leaves the preset list, so a provider is one
        // line in the picker, not two that mean different things — and the
        // next plugin (Outlook) is a line here the day its module is.
        let plugins = self.plugin_providers();
        // Everything the plugin branch of the form says, read before the
        // form is borrowed for drawing: a click on Sign in is a flag the
        // end of the page acts on, once the borrow is over.
        let plugin_warning = self
            .account_form
            .plugin
            .as_deref()
            .filter(|id| self.plugins.provider(id).is_none())
            .map(|id| {
                format!(
                    "The {id} provider plugin is not installed or enabled — enable it in                      Plugins and restart, and this account works again."
                )
            });
        let mut sign_in_clicked = false;
        let providers = self
            .enabled_vendors()
            .into_iter()
            .filter(|key| {
                !plugins
                    .iter()
                    .any(|(id, _)| vendor::VendorKey::for_provider(id) == Some(*key))
            })
            .collect::<Vec<_>>();
        // A sign-in that failed says so here, on the form it started from —
        // not only on the Accounts page the window already left. While the
        // browser is the thing happening, the waiting note below says that
        // instead.
        if self.plugin_signing_in.is_none() {
            if let Some(error) = &self.account_error {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("Sign-in failed: {error}"),
                );
                ui.add_space(4.0);
            }
        }
        let f = &mut self.account_form;
        egui::Grid::new("account_form_grid")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Provider");
                let selected = match &f.plugin {
                    Some(id) => plugins
                        .iter()
                        .find(|(plugin_id, _)| plugin_id == id)
                        .map(|(_, name)| name.clone())
                        .unwrap_or_else(|| id.clone()),
                    None => f.vendor.label().to_string(),
                };
                egui::ComboBox::from_id_salt("account_provider")
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for key in providers.iter().copied() {
                            if ui
                                .selectable_label(
                                    f.plugin.is_none() && f.vendor == key,
                                    key.label(),
                                )
                                .clicked()
                            {
                                f.plugin = None;
                                f.vendor = key;
                                f.apply_preset();
                            }
                        }
                        for (id, name) in plugins.iter() {
                            if ui
                                .selectable_label(f.plugin.as_deref() == Some(id.as_str()), name)
                                .clicked()
                            {
                                f.apply_plugin(id);
                            }
                        }
                    });
                ui.end_row();

                ui.label("Account name");
                ui.add(Field::new(
                    egui::TextEdit::singleline(&mut f.name).hint_text("Personal mail"),
                ));
                ui.end_row();

                // A plugin account is its address and a browser; the module
                // owns every endpoint and both logins.
                if f.plugin.is_some() {
                    ui.label("Email");
                    ui.add(Field::new(
                        egui::TextEdit::singleline(&mut f.username).hint_text("you@gmail.com"),
                    ));
                    ui.end_row();
                } else {
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
                }
            });

        if f.plugin.is_some() {
            // The plugin's own half of the form: the deployment's client id
            // and the browser sign-in. An unavailable module is the one thing
            // that can stop the account working from here, so it is said
            // plainly rather than left for the sync to fail on.
            if let Some(warning) = &plugin_warning {
                ui.colored_label(ui.visuals().warn_fg_color, warning);
            }
            ui.add_space(4.0);
            egui::Grid::new("account_plugin_grid")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label("OAuth client id");
                    ui.add(Field::new(
                        egui::TextEdit::singleline(&mut f.client_id)
                            .hint_text("the deployment's own client, from the provider's console"),
                    ));
                    ui.end_row();

                    // Google issues a desktop client a secret and refuses the
                    // token exchange without it; the field stays optional
                    // because a public client (Entra) has none to give.
                    ui.label("OAuth client secret");
                    ui.add(Field::new(
                        egui::TextEdit::singleline(&mut f.client_secret)
                            .password(true)
                            .hint_text("only if the provider's client has one (a Google desktop client does)"),
                    ));
                    ui.end_row();
                });
            ui.add_space(6.0);
            let signed_in = f
                .editing
                .as_deref()
                .and_then(|id| self.accounts.iter().find(|account| account.id == id))
                .is_some_and(|account| account.access_token.is_some());
            let waiting = self.plugin_signing_in.is_some();
            // A sign-in without a client id is refused by the module before
            // any browser opens — an outcome that must not be reachable as a
            // click that appears to do nothing. The id field is right above.
            let has_client_id = !f.client_id.trim().is_empty();
            ui.horizontal(|ui| {
                let who = match f.plugin.as_deref() {
                    Some("gmail") => "Google",
                    Some("outlook") => "Microsoft",
                    _ => "the provider",
                };
                let label = if waiting {
                    "Signing in…".to_string()
                } else if signed_in {
                    "Sign in again".to_string()
                } else {
                    format!("Sign in with {who}")
                };
                let enabled = !waiting && has_client_id;
                if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                    sign_in_clicked = true;
                }
                ui.label(
                    egui::RichText::new(if waiting {
                        "The browser should have opened — finish there and this window follows."
                    } else if !has_client_id {
                        "An OAuth client id is required first: register an application with the \
                         provider and paste its id above."
                    } else if signed_in {
                        "Signed in — the browser holds the password, Kimo holds only tokens."
                    } else {
                        "Opens your browser to sign in; Kimo never sees the password."
                    })
                    .small()
                    .weak(),
                );
            });
        } else {
            // What the provider's sign-in needs. It is advice, not a gate: the
            // fields stay editable and Save is enabled, because a server that is
            // not quite what its host suggests is the user's to configure.
            let profile = f.vendor.profile();
            if let Some(note) = profile.note {
                let text = egui::RichText::new(note).small();
                if profile.auth == vendor::AuthKind::OAuthOnly {
                    let color = ui.visuals().warn_fg_color;
                    ui.colored_label(color, text);
                } else {
                    ui.label(text.weak());
                }
            }

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
                            egui::TextEdit::singleline(&mut f.smtp_host)
                                .hint_text("smtp.example.com"),
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
                                f.smtp_port_str =
                                    SmtpSettings::default_port(f.smtp_tls).to_string();
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
        }

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

        // Deferred from the button above: the click happened while the form
        // was borrowed for drawing, the sign-in needs the whole app.
        if sign_in_clicked {
            self.start_plugin_sign_in();
        }
    }
    /// Preferences → Plugins: what the startup scan found. A disabled plugin
    /// is listed from its manifest but its module was not compiled or
    /// instantiated; the switch takes effect at the next start.
    pub(super) fn plugins_page(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("WASM plugins").strong());
        ui.label(
            egui::RichText::new(
                "Kimo loads .wasm modules only. A plugin is trusted enough to read the mail and use the network permissions shown here.",
            )
            .small()
            .weak(),
        );
        ui.add_space(6.0);

        for error in self.plugins.errors() {
            ui.label(
                egui::RichText::new(format!("Not loaded: {error}"))
                    .color(ui.visuals().error_fg_color),
            );
        }

        let mut rows: Vec<(String, String, String, String, bool)> = Vec::new();
        for loaded in self.plugins.plugins() {
            let permissions = loaded.plugin.permissions().names();
            let permissions = if permissions.is_empty() {
                "no network or secrets".to_string()
            } else {
                permissions.join(", ")
            };
            rows.push((
                loaded.manifest.name.clone(),
                loaded.manifest.version.clone(),
                loaded.manifest.id.clone(),
                format!(
                    "{} · {permissions} · {} MiB cap",
                    loaded.manifest.module,
                    loaded.plugin.limits().memory_bytes / (1024 * 1024)
                ),
                true,
            ));
        }
        for manifest in self.plugins.disabled() {
            rows.push((
                manifest.name.clone(),
                manifest.version.clone(),
                manifest.id.clone(),
                format!("{} · not compiled at startup", manifest.module),
                false,
            ));
        }

        if rows.is_empty() {
            ui.label("No plugins found in the plugin directories.");
            return;
        }

        let mut changed = false;
        for (name, version, id, detail, enabled) in rows {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&name).strong());
                    ui.label(egui::RichText::new(format!("v{version}")).small().weak());
                    let mut on = enabled;
                    if ui
                        .checkbox(&mut on, "Enabled")
                        .on_hover_text("Takes effect at the next start")
                        .changed()
                    {
                        if on {
                            self.disabled_plugins.remove(&id);
                        } else {
                            self.disabled_plugins.insert(id.clone());
                        }
                        changed = true;
                    }
                });
                ui.label(
                    egui::RichText::new(format!("{} · {detail}", id))
                        .small()
                        .weak(),
                );
            });
            ui.add_space(2.0);
        }
        if changed {
            self.persist_settings();
        }
    }
    /// Preferences → General: the mail list, the automatic sync, the
    /// notifications and the window's minimize and close behaviour, all
    /// persisted on change.
    pub(super) fn general_page(&mut self, ui: &mut egui::Ui) {
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

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        ui.label(egui::RichText::new("Window").strong());
        if ui
            .checkbox(&mut self.minimize_to_tray, "Minimize to the tray")
            .changed()
        {
            self.persist_settings();
        }
        ui.label(
            egui::RichText::new(
                "The minimize button hides Kimo instead of sending it to the task bar; \
                 click the tray icon, or its Show item, to bring it back. Without a tray \
                 icon, minimize keeps its normal behaviour.",
            )
            .small()
            .weak(),
        );

        ui.add_space(8.0);
        if ui
            .checkbox(&mut self.close_to_tray, "Close to the tray")
            .changed()
        {
            self.persist_settings();
        }
        ui.label(
            egui::RichText::new(
                "The close button, and the window manager's close, hide Kimo instead of \
                 quitting; choose Quit from the tray icon's menu to end it. Without a tray \
                 icon, closing quits as usual.",
            )
            .small()
            .weak(),
        );
    }
    /// Preferences → Appearance: theme, accent and HTML renderer, all
    /// persisted to settings.json on change.
    pub(super) fn appearance_page(&mut self, ui: &mut egui::Ui) {
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
                fonts_changed |= font_picker(
                    ui,
                    "ui-font",
                    &self.font_families,
                    &mut self.ui_font,
                    "Default",
                );
                ui.end_row();

                ui.label("Fixed-width");
                fonts_changed |= font_picker(
                    ui,
                    "mono-font",
                    &self.font_families,
                    &mut self.mono_font,
                    "Default",
                );
                ui.end_row();

                // The fallback, not an override: mail that names its own
                // family still wins, and the picked face is appended last.
                ui.label("CJK fallback");
                fonts_changed |= font_picker(
                    ui,
                    "cjk-font",
                    &self.font_families,
                    &mut self.cjk_font,
                    "Automatic",
                );
                ui.end_row();
            });
        // What the pick does not cover, said rather than filtered: a reader who
        // picks a Latin family has to see why their mail turned into boxes. The
        // scripts are named rather than drawn — the sample characters are
        // exactly what this face cannot show, so the warning would be boxes too.
        if let Some((name, missing)) = &self.cjk_missing {
            if !missing.is_empty() {
                let scripts: Vec<&str> = missing
                    .iter()
                    .map(|character| crate::fonts::script_of_sample(*character))
                    .collect();
                ui.label(
                    egui::RichText::new(format!(
                        "{} has no glyphs for {}; CJK mail will show boxes there.",
                        name,
                        scripts.join(", ")
                    ))
                    .small()
                    .color(ui.visuals().warn_fg_color),
                );
            }
        }
        // A name from a settings.json this machine may not have: rendering
        // falls through to the automatic face, and saying so is better than a
        // picker that names nothing installed.
        let unresolved_cjk = match (&self.cjk_font, &self.font_families) {
            (Some(name), Some(families)) => crate::fonts::find(families, name).is_none(),
            _ => false,
        };
        if unresolved_cjk {
            ui.label(
                egui::RichText::new(
                    "That family is not installed; the automatic fallback is used.",
                )
                .small()
                .weak(),
            );
        }
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
                "Interface and fixed-width apply to the app UI and to monospace text in the \
                 Simple renderer; the CJK fallback also covers CJK mail there. Blitz mail \
                 keeps its own font set.",
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
    /// Preferences → Graphics: which of the machine's cards Kimo draws with.
    ///
    /// The choice belongs to the *next* start, and the page says so: the
    /// Vulkan drivers are opened when the instance is made, and the ones a card
    /// does not need are exactly what this page keeps out of the process
    /// (`crate::gpus`). The scan is a worker job — sysfs, the manifest
    /// directories and `pci.ids` are filesystem work — so the page draws a
    /// spinner until it answers rather than reading them itself.
    pub(super) fn graphics_page(&mut self, ui: &mut egui::Ui) {
        if self.gpu_scan.is_none() {
            if !self.gpu_scanning {
                self.gpu_scanning = true;
                sync::spawn(Job::ScanGpus, self.ctx.clone(), self.tx.clone());
            }
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().color(theme::of(ui.ctx()).accent));
                ui.label(
                    egui::RichText::new("Looking for graphics cards…")
                        .small()
                        .weak(),
                );
            });
            return;
        }

        ui.label(egui::RichText::new("Graphics card").strong());
        ui.add_space(4.0);

        // The click is collected and applied after the list: the radios read
        // `self.gpu`, and a pick must not change the page mid-draw.
        let mut pick: Option<Option<String>> = None;
        {
            let scan = self.gpu_scan.as_ref().expect("just checked");
            if scan.cards.is_empty() {
                ui.label(
                    egui::RichText::new(
                        "No graphics card with a kernel driver was found on this machine.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(4.0);
            }
            if ui.radio(self.gpu.is_none(), "Automatic").clicked() {
                pick = Some(None);
            }
            ui.add_space(4.0);
            for card in &scan.cards {
                let selected = self
                    .gpu
                    .as_deref()
                    .is_some_and(|saved| saved.eq_ignore_ascii_case(&card.pci));
                if ui.radio(selected, card.title(&scan.names)).clicked() {
                    pick = Some(Some(card.pci.clone()));
                }
                ui.label(
                    egui::RichText::new(card.subtitle(&scan.names))
                        .small()
                        .weak(),
                );
                ui.add_space(4.0);
            }
        }
        if let Some(pick) = pick {
            self.gpu = pick;
            self.persist_settings();
        }

        let scan = self.gpu_scan.as_ref().expect("just checked");
        let plan = crate::gpus::plan(self.gpu.as_deref(), &scan.cards, &scan.icds, scan.locked);
        match &plan.status {
            Status::Automatic => {
                let text = if plan.all.is_empty() {
                    "No Vulkan driver is installed here; Kimo uses its GL fallback.".to_string()
                } else {
                    format!(
                        "The Vulkan loader opens all {} installed drivers.",
                        plan.all.len()
                    )
                };
                ui.label(egui::RichText::new(text).small().weak());
            }
            Status::Picked(_) => match plan.restriction() {
                Some(drivers) => {
                    let left = plan.all.len() - drivers.len();
                    ui.label(
                        egui::RichText::new(format!(
                            "Loads {} of the {} installed Vulkan drivers ({}). The other {} {} \
                             not loaded.",
                            drivers.len(),
                            plan.all.len(),
                            crate::gpus::manifest_names(drivers),
                            left,
                            if left == 1 { "is" } else { "are" },
                        ))
                        .small()
                        .weak(),
                    );
                }
                None => {
                    ui.label(
                        egui::RichText::new(
                            "Every installed Vulkan driver serves this card, so nothing is \
                             left out.",
                        )
                        .small()
                        .weak(),
                    );
                }
            },
            Status::NotPresent(saved) => {
                ui.label(
                    egui::RichText::new(format!(
                        "The GPU saved in settings.json ({saved}) is not installed here; every \
                         Vulkan driver is loaded instead."
                    ))
                    .small()
                    .color(ui.visuals().warn_fg_color),
                );
            }
            Status::Malformed(saved) => {
                ui.label(
                    egui::RichText::new(format!(
                        "settings.json names a GPU Kimo does not recognise ({saved:?}); every \
                         Vulkan driver is loaded instead."
                    ))
                    .small()
                    .color(ui.visuals().warn_fg_color),
                );
            }
            Status::NoDriver(card) => {
                ui.label(
                    egui::RichText::new(format!(
                        "No Vulkan driver is installed for {} ({}); every Vulkan driver is \
                         loaded instead.",
                        card.title(&scan.names),
                        card.pci,
                    ))
                    .small()
                    .color(ui.visuals().warn_fg_color),
                );
            }
            // Said by the note below: the session's own list is not a failure
            // of the pick, and drawing it twice would read like one.
            Status::Environment => {}
        }
        if scan.locked {
            ui.label(
                egui::RichText::new(
                    "This session already names the Vulkan drivers (VK_DRIVER_FILES), so the \
                     choice above is not applied.",
                )
                .small()
                .weak(),
            );
        }
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "Kimo chooses its GPU when it starts. A card you pick opens only that card's \
                 Vulkan drivers, so a card you do not use never enters this process. A change \
                 applies the next time Kimo starts.",
            )
            .small()
            .weak(),
        );
    }
    /// Preferences → About: the app's icon, the version it was built as, and
    /// what it is. The icon is the embedded PNG the tray and notifications
    /// serve, uploaded once the first time this page is drawn; the version is
    /// the build's own `CARGO_PKG_VERSION`, so the page cannot drift from
    /// Cargo.toml.
    pub(super) fn about_page(&mut self, ui: &mut egui::Ui) {
        if self.app_icon.is_none() {
            self.app_icon = load_app_icon(ui.ctx());
        }
        ui.horizontal(|ui| {
            if let Some(icon) = &self.app_icon {
                let (rect, _) = ui
                    .allocate_exact_size(egui::Vec2::splat(ABOUT_ICON_SIZE), egui::Sense::hover());
                ui.painter().image(
                    icon.id(),
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
                ui.add_space(ABOUT_ICON_GAP);
            }
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Kimo").heading());
                ui.label(
                    egui::RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION")))
                        .small()
                        .weak(),
                );
            });
        });
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(egui::RichText::new("A desktop email client written in pure Rust").strong());
        ui.add_space(4.0);
        ui.label(
            "Kimo reads mail over IMAP4rev1 and POP3, sends it over SMTP, and keeps its \
             accounts, folders and cached mail in one local SQLite database. The interface \
             is drawn with egui, TLS is rustls with the ring provider, and HTML mail renders \
             either as native widgets or as a Blitz-rasterized page.",
        );
        ui.add_space(12.0);
        ui.label(
            egui::RichText::new(
                "No async runtime, no JavaScript, and no C library beyond the bundled SQLite \
                 the mail database uses.",
            )
            .small()
            .weak(),
        );
    }
}
