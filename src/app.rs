//! The egui application: all UI state and drawing. Network results arrive as
//! `sync::Event`s drained at the top of every frame; opening a socket happens
//! only in worker threads.
//!
//! Layout: custom title bar on top (nav toggle, account menu, sync, window
//! controls), then three columns — collapsible navigation, message list,
//! reading pane.

use std::collections::HashMap;
use std::sync::mpsc;

use crate::models::{Account, MailMessage, Protocol, TlsMode};
use crate::storage::{self, FolderCache};
use crate::sync::{self, Event, Job};

/// Below this window width the navigation bar collapses automatically.
const NAV_AUTO_COLLAPSE_WIDTH: f32 = 640.0;

pub struct JimaoApp {
    ctx: egui::Context,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    accounts: Vec<Account>,
    states: HashMap<String, AccountUiState>,
    selected_account: Option<String>,
    selected_folder: Option<String>,
    selected_message: Option<String>,
    filter: String,
    show_add_account: bool,
    add_form: AddAccountForm,
    confirm_delete: Option<String>,
    nav_collapsed: bool,
    /// Whether the window was narrow last frame, so auto-collapse only
    /// triggers when crossing the threshold (not every frame).
    was_narrow: bool,
}

#[derive(Default)]
pub struct AccountUiState {
    pub folders: Vec<String>,
    pub cache: storage::AccountCache,
    pub syncing: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
struct AddAccountForm {
    name: String,
    protocol: Protocol,
    host: String,
    port_str: String,
    username: String,
    password: String,
    tls: TlsMode,
}

impl Default for AddAccountForm {
    fn default() -> Self {
        Self {
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

impl JimaoApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_font(&cc.egui_ctx);

        let accounts = storage::load_accounts();
        let mut states = HashMap::new();
        for account in &accounts {
            let state = AccountUiState {
                cache: storage::load_cache(&account.id),
                ..AccountUiState::default()
            };
            states.insert(account.id.clone(), state);
        }

        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            ctx: cc.egui_ctx.clone(),
            tx,
            rx,
            accounts,
            states,
            selected_account: None,
            selected_folder: None,
            selected_message: None,
            filter: String::new(),
            show_add_account: false,
            add_form: AddAccountForm::default(),
            confirm_delete: None,
            nav_collapsed: false,
            was_narrow: false,
        };

        app.selected_account = app.accounts.first().map(|a| a.id.clone());
        app.selected_folder = Some("INBOX".to_string());

        for account in app.accounts.clone() {
            sync::spawn(Job::Sync { account }, app.ctx.clone(), app.tx.clone());
        }
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
                    state.folders = folders;
                }
            }
            Event::Messages {
                account_id,
                folder,
                uidvalidity,
                messages,
            } => {
                let mut snapshot = None;
                if let Some(state) = self.states.get_mut(&account_id) {
                    if state.cache.folders.iter().all(|f| f.name != folder) {
                        state.cache.folders.push(FolderCache {
                            name: folder.clone(),
                            uidvalidity: None,
                            synced: false,
                            messages: Vec::new(),
                        });
                    }
                    let fc = state
                        .cache
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
                    snapshot = Some(state.cache.clone());
                }
                if let Some(cache) = snapshot {
                    let _ = storage::save_cache(&account_id, &cache);
                }
            }
            Event::SyncDone { account_id } => {
                let mut snapshot = None;
                if let Some(state) = self.states.get_mut(&account_id) {
                    state.syncing = false;
                    snapshot = Some(state.cache.clone());
                }
                if let Some(cache) = snapshot {
                    let _ = storage::save_cache(&account_id, &cache);
                }
            }
            Event::SyncFailed { account_id, error } => {
                if let Some(state) = self.states.get_mut(&account_id) {
                    state.syncing = false;
                    state.error = Some(error);
                }
            }
        }
    }

    // ----- actions ----------------------------------------------------------

    fn sync_all(&mut self) {
        for account in self.accounts.clone() {
            sync::spawn(Job::Sync { account }, self.ctx.clone(), self.tx.clone());
        }
    }

    fn current_account(&self) -> Option<Account> {
        self.accounts
            .iter()
            .find(|a| self.selected_account.as_deref() == Some(a.id.as_str()))
            .cloned()
    }

    fn select_folder(&mut self, account_id: String, folder: String, protocol: Protocol) {
        self.selected_account = Some(account_id.clone());
        self.selected_folder = Some(folder.clone());
        self.selected_message = None;

        let needs_fetch = match self.states.get(&account_id) {
            Some(state) => match state.cache.folders.iter().find(|f| f.name == folder) {
                Some(fc) => !fc.synced,
                None => true,
            },
            None => false,
        };
        if !needs_fetch {
            return;
        }
        if let Some(account) = self.accounts.iter().find(|a| a.id == account_id).cloned() {
            let job = match protocol {
                Protocol::Imap => Job::SyncFolder { account, folder },
                Protocol::Pop3 => Job::Sync { account },
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

        // Mark seen locally first so the UI reacts immediately.
        let mut snapshot = None;
        if let Some(state) = self.states.get_mut(&account_id) {
            if let Some(fc) = state.cache.folders.iter_mut().find(|f| f.name == folder) {
                if let Some(existing) = fc.messages.iter_mut().find(|m| m.uid == msg.uid) {
                    existing.seen = true;
                    snapshot = Some(state.cache.clone());
                }
            }
        }
        if let Some(cache) = snapshot {
            let _ = storage::save_cache(&account_id, &cache);
        }

        // Then push the flag to the server (IMAP only; POP3 is local-only).
        if let Some(account) = self.accounts.iter().find(|a| a.id == account_id).cloned() {
            if account.protocol == Protocol::Imap {
                if let Ok(uid) = msg.uid.parse::<u32>() {
                    sync::spawn(
                        Job::MarkSeen {
                            account,
                            folder,
                            uid,
                        },
                        self.ctx.clone(),
                        self.tx.clone(),
                    );
                }
            }
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
        let _ = storage::save_accounts(&self.accounts);
        storage::delete_cache(account_id);
    }

    fn save_new_account(&mut self) {
        let form = self.add_form.clone();
        let port: u16 = form
            .port_str
            .trim()
            .parse()
            .unwrap_or_else(|_| form.protocol.default_port(form.tls));

        let base = slug(&form.name);
        let mut id = base.clone();
        let mut n = 1;
        while self.accounts.iter().any(|a| a.id == id) {
            n += 1;
            id = format!("{base}-{n}");
        }

        let account = Account {
            id,
            name: form.name.trim().to_string(),
            protocol: form.protocol,
            host: form.host.trim().to_string(),
            port,
            username: form.username.trim().to_string(),
            password: form.password.clone(),
            tls: form.tls,
        };
        let account_id = account.id.clone();
        self.accounts.push(account.clone());
        self.states.entry(account_id.clone()).or_default();
        let _ = storage::save_accounts(&self.accounts);

        sync::spawn(Job::Sync { account }, self.ctx.clone(), self.tx.clone());
        self.selected_account = Some(account_id);
        self.selected_folder = Some("INBOX".to_string());
        self.selected_message = None;
        self.show_add_account = false;
    }

    // ----- queries ----------------------------------------------------------

    /// Messages of the currently selected folder, filtered and sorted for display.
    fn current_messages(&self) -> Vec<MailMessage> {
        let Some(account_id) = &self.selected_account else {
            return Vec::new();
        };
        let Some(folder) = &self.selected_folder else {
            return Vec::new();
        };
        let Some(state) = self.states.get(account_id) else {
            return Vec::new();
        };
        let Some(fc) = state.cache.folders.iter().find(|f| f.name == *folder) else {
            return Vec::new();
        };
        let needle = self.filter.trim().to_lowercase();
        fc.messages
            .iter()
            .filter(|m| {
                needle.is_empty()
                    || m.subject.to_lowercase().contains(&needle)
                    || m.from.to_lowercase().contains(&needle)
            })
            .cloned()
            .collect()
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
            .cache
            .folders
            .iter()
            .find(|f| f.name == *folder)
            .is_some_and(|f| f.synced)
    }

    fn selected_message_view(&self) -> Option<MailMessage> {
        let uid = self.selected_message.as_ref()?;
        self.current_messages().into_iter().find(|m| m.uid == *uid)
    }

    // ----- UI ----------------------------------------------------------------

    /// Custom window title bar (the window is created undecorated in
    /// `main.rs`). Left to right: nav collapse toggle, app title, account
    /// menu, sync button, per-account status. Right edge: minimize /
    /// maximize / close. The leftover space drags the window (double-click
    /// toggles maximize).
    fn titlebar(&mut self, ui: &mut egui::Ui) {
        let bar = ui.available_rect_before_wrap();
        let visuals = ui.visuals().clone();
        let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));

        #[derive(Clone, Copy)]
        enum Ctrl {
            Minimize,
            Maximize,
            Close,
        }
        // Drawn right to left so Close ends up at the very right edge.
        let buttons = [
            (Ctrl::Close, false),
            (Ctrl::Maximize, maximized),
            (Ctrl::Minimize, false),
        ];
        for (idx, (ctrl, is_max)) in buttons.iter().enumerate() {
            let right = bar.right() - TITLEBAR_BUTTON_WIDTH * idx as f32;
            let rect = egui::Rect::from_min_max(
                egui::pos2(right - TITLEBAR_BUTTON_WIDTH, bar.top()),
                egui::pos2(right, bar.bottom()),
            );
            let resp = ui.interact(
                rect,
                egui::Id::new(("titlebar-button", idx)),
                egui::Sense::click(),
            );
            let fg = if resp.hovered() {
                let bg = match ctrl {
                    Ctrl::Close => egui::Color32::from_rgb(232, 17, 35),
                    _ => visuals.widgets.hovered.bg_fill,
                };
                ui.painter().rect_filled(rect, 0.0, bg);
                egui::Color32::WHITE
            } else {
                visuals.text_color()
            };
            match (ctrl, is_max) {
                (Ctrl::Minimize, _) => paint_minimize_icon(ui.painter(), rect, fg),
                (Ctrl::Maximize, false) => paint_maximize_icon(ui.painter(), rect, fg),
                (Ctrl::Maximize, true) => {
                    paint_restore_icon(ui.painter(), rect, fg, visuals.panel_fill)
                }
                (Ctrl::Close, _) => paint_close_icon(ui.painter(), rect, fg),
            }
            if resp.clicked() {
                match ctrl {
                    Ctrl::Minimize => ui
                        .ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
                    Ctrl::Maximize => ui
                        .ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized)),
                    Ctrl::Close => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                }
            }
        }

        // --- left content row, kept clear of the window buttons ---
        let content_right = bar.right() - TITLEBAR_BUTTON_WIDTH * buttons.len() as f32;
        let row_size = egui::vec2((content_right - bar.left()).max(0.0), bar.height());
        let content_end = ui
            .allocate_ui_with_layout(
                row_size,
                egui::Layout::left_to_right(egui::Align::Center),
                |row| {
                    // Nav collapse toggle (hamburger, painted as lines).
                    let toggle = row.add_sized([26.0, 22.0], egui::Button::new(""));
                    if toggle.clicked() {
                        self.nav_collapsed = !self.nav_collapsed;
                    }
                    let tr = toggle.rect;
                    for i in -1i32..=1 {
                        let y = tr.center().y + i as f32 * 4.0;
                        row.painter().line_segment(
                            [
                                egui::pos2(tr.left() + 6.0, y),
                                egui::pos2(tr.right() - 6.0, y),
                            ],
                            egui::Stroke::new(1.5_f32, visuals.text_color()),
                        );
                    }
                    toggle.on_hover_text(if self.nav_collapsed {
                        "Show folders"
                    } else {
                        "Hide folders"
                    });

                    row.label(egui::RichText::new("Jimao").small().strong());
                    row.separator();

                    // Account menu: switch accounts, add, remove (two-step confirm).
                    let selected_name = self
                        .current_account()
                        .map(|a| a.name)
                        .unwrap_or_else(|| "No account".to_string());
                    let mut switch_to: Option<Account> = None;
                    let mut add_clicked = false;
                    egui::ComboBox::from_id_salt("account-selector")
                        .selected_text(selected_name)
                        .width(170.0)
                        .show_ui(row, |ui| {
                            ui.set_min_width(240.0);
                            for account in &self.accounts {
                                let is_sel =
                                    self.selected_account.as_deref() == Some(account.id.as_str());
                                let text = egui::RichText::new(format!(
                                    "{} · {}",
                                    account.name,
                                    account.protocol.label()
                                ));
                                if ui.selectable_label(is_sel, text).clicked() {
                                    switch_to = Some(account.clone());
                                }
                            }
                            if !self.accounts.is_empty() {
                                ui.separator();
                            }
                            if ui.button("+ Add account…").clicked() {
                                add_clicked = true;
                            }
                            if let Some(current) = self
                                .accounts
                                .iter()
                                .find(|a| self.selected_account.as_deref() == Some(a.id.as_str()))
                            {
                                let confirming =
                                    self.confirm_delete.as_deref() == Some(current.id.as_str());
                                let label = if confirming {
                                    egui::RichText::new(format!("Remove \"{}\"?", current.name))
                                        .color(egui::Color32::RED)
                                } else {
                                    egui::RichText::new(format!("Remove \"{}\"", current.name))
                                };
                                if ui.button(label).clicked() {
                                    if confirming {
                                        let id = current.id.clone();
                                        self.delete_account(&id);
                                        self.confirm_delete = None;
                                    } else {
                                        self.confirm_delete = Some(current.id.clone());
                                    }
                                }
                            }
                        });
                    if let Some(account) = switch_to {
                        self.select_folder(
                            account.id.clone(),
                            "INBOX".to_string(),
                            account.protocol,
                        );
                    }
                    if add_clicked {
                        self.add_form = AddAccountForm::default();
                        self.show_add_account = true;
                    }

                    if row.button("Sync").clicked() {
                        self.sync_all();
                    }

                    // Per-account status: spinner while syncing, warning on error.
                    for account in self.accounts.clone() {
                        let Some(state) = self.states.get(&account.id) else {
                            continue;
                        };
                        if state.syncing {
                            row.spinner()
                                .on_hover_text(format!("{}: syncing…", account.name));
                        } else if let Some(err) = &state.error {
                            row.label(egui::RichText::new("⚠").color(egui::Color32::RED))
                                .on_hover_text(format!("{}: {err}", account.name));
                        }
                    }

                    row.min_rect().right()
                },
            )
            .inner;

        // --- drag surface: whatever is left between content and buttons ---
        let drag_left = content_end + 4.0;
        let drag_rect = egui::Rect::from_min_max(
            egui::pos2(drag_left.max(bar.left()), bar.top()),
            egui::pos2(content_right, bar.bottom()),
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

        ui.painter().line_segment(
            [bar.left_bottom(), bar.right_bottom()],
            egui::Stroke::new(1.0_f32, visuals.widgets.noninteractive.bg_stroke.color),
        );
    }

    /// Left column: folders of the selected account.
    fn nav(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let Some(account) = self.current_account() else {
                    ui.label("No account selected.");
                    ui.label(
                        egui::RichText::new("Add one from the account menu in the title bar.")
                            .small()
                            .weak(),
                    );
                    return;
                };
                let (folders, syncing, error) = {
                    let state = self.states.entry(account.id.clone()).or_default();
                    (state.folders.clone(), state.syncing, state.error.clone())
                };

                ui.label(egui::RichText::new("MAILBOXES").small().weak());
                ui.add_space(4.0);

                let folders_were_empty = folders.is_empty();
                let folder_list = if folders.is_empty() {
                    vec!["INBOX".to_string()]
                } else {
                    folders
                };
                for folder in folder_list {
                    let selected = self.selected_account.as_deref() == Some(account.id.as_str())
                        && self.selected_folder.as_deref() == Some(folder.as_str());
                    let unread = self
                        .states
                        .get(&account.id)
                        .and_then(|s| s.cache.folders.iter().find(|f| f.name == folder))
                        .map(|f| f.messages.iter().filter(|m| !m.seen).count())
                        .unwrap_or(0);
                    let text = if unread > 0 {
                        egui::RichText::new(format!("📁 {folder} ({unread})")).strong()
                    } else {
                        egui::RichText::new(format!("📁 {folder}")).weak()
                    };
                    if ui.selectable_label(selected, text).clicked() {
                        self.select_folder(account.id.clone(), folder, account.protocol);
                    }
                }
                if syncing && folders_were_empty {
                    ui.add_space(4.0);
                    ui.spinner();
                }
                if let Some(err) = error {
                    ui.add_space(4.0);
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                    ui.label(
                        egui::RichText::new(format!("⚠ {err}"))
                            .small()
                            .color(egui::Color32::RED),
                    );
                }
            });
    }

    /// Middle column: filter bar plus the message list.
    fn mail_list(&mut self, ui: &mut egui::Ui) {
        if self.accounts.is_empty() || self.selected_account.is_none() {
            ui.centered_and_justified(|ui| {
                ui.heading("Jimao");
            });
            return;
        }

        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("Filter by subject or sender…")
                    .desired_width(200.0),
            );
            let count = self.current_messages().len();
            ui.label(
                egui::RichText::new(format!("{count} messages"))
                    .small()
                    .weak(),
            );
        });
        ui.separator();

        let messages = self.current_messages();
        if messages.is_empty() {
            ui.centered_and_justified(|ui| {
                let text = if !self.filter.is_empty() {
                    "No messages match the filter."
                } else if self.selected_folder_synced() {
                    "This folder is empty."
                } else {
                    "Not fetched yet — click Sync in the title bar."
                };
                ui.label(egui::RichText::new(text).weak().heading());
            });
            return;
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("message_list")
                    .num_columns(3)
                    .striped(true)
                    .min_col_width(80.0)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("From").small().weak());
                        ui.label(egui::RichText::new("Subject").small().weak());
                        ui.label(egui::RichText::new("Date").small().weak());
                        ui.end_row();
                        for msg in &messages {
                            let is_selected =
                                self.selected_message.as_deref() == Some(msg.uid.as_str());
                            let (style_from, style_subject) = if msg.seen {
                                (
                                    egui::RichText::new(truncate(&msg.from, 24)),
                                    egui::RichText::new(truncate(&msg.subject, 50)),
                                )
                            } else {
                                (
                                    egui::RichText::new(truncate(&msg.from, 24)).strong(),
                                    egui::RichText::new(truncate(&msg.subject, 50)).strong(),
                                )
                            };
                            if ui.selectable_label(is_selected, style_from).clicked() {
                                self.open_message(msg);
                            }
                            if ui.selectable_label(is_selected, style_subject).clicked() {
                                self.open_message(msg);
                            }
                            if ui
                                .selectable_label(
                                    is_selected,
                                    egui::RichText::new(display_date(msg)).small().weak(),
                                )
                                .clicked()
                            {
                                self.open_message(msg);
                            }
                            ui.end_row();
                        }
                    });
            });
    }

    /// Right column: the selected message, or a placeholder.
    fn mail_content(&mut self, ui: &mut egui::Ui) {
        let Some(msg) = self.selected_message_view() else {
            ui.centered_and_justified(|ui| {
                ui.label(egui::RichText::new("No mail selected").weak().heading());
            });
            return;
        };
        ui.vertical(|ui| {
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
                    egui::RichText::new(format!("📎 {}", msg.attachment_names.join(", "))).small(),
                );
            }
            ui.separator();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                    if let Some(html) = &msg.body_html {
                        crate::html::render(ui, html);
                    } else if !msg.body_text.is_empty() {
                        ui.label(msg.body_text.as_str());
                    } else {
                        ui.label(egui::RichText::new("(no text body)").weak());
                    }
                });
        });
    }

    fn add_account_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_add_account;
        egui::Window::new("Add account")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                let f = &mut self.add_form;
                egui::Grid::new("add_account_grid")
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
                        ui.add(
                            egui::TextEdit::singleline(&mut f.host).hint_text("imap.example.com"),
                        );
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
                        ui.add(
                            egui::TextEdit::singleline(&mut f.username)
                                .hint_text("you@example.com"),
                        );
                        ui.end_row();

                        ui.label("Password");
                        ui.add(egui::TextEdit::singleline(&mut f.password).password(true));
                        ui.end_row();
                    });

                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "Note: credentials are stored in plain text on this machine.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(4.0);

                let valid = {
                    let f = &self.add_form;
                    !f.name.trim().is_empty()
                        && !f.host.trim().is_empty()
                        && !f.username.trim().is_empty()
                        && f.port_str.trim().parse::<u16>().is_ok()
                };
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.show_add_account = false;
                    }
                    if ui
                        .add_enabled(valid, egui::Button::new("Save & connect"))
                        .clicked()
                    {
                        self.save_new_account();
                    }
                });
            });
        if !open {
            self.show_add_account = false;
        }
    }
}

impl eframe::App for JimaoApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        // Collapse the nav bar automatically when the window becomes narrow;
        // re-expanding is a manual choice and stays until the next crossing.
        let narrow = ctx.screen_rect().width() < NAV_AUTO_COLLAPSE_WIDTH;
        if narrow && !self.was_narrow {
            self.nav_collapsed = true;
        }
        self.was_narrow = narrow;

        egui::TopBottomPanel::top("titlebar")
            .exact_height(TITLEBAR_HEIGHT)
            .show(ctx, |ui| self.titlebar(ui));
        egui::SidePanel::right("mail_content")
            .resizable(true)
            .min_width(280.0)
            .default_width(460.0)
            .show(ctx, |ui| self.mail_content(ui));
        if !self.nav_collapsed {
            egui::SidePanel::left("nav")
                .resizable(true)
                .min_width(150.0)
                .default_width(200.0)
                .show(ctx, |ui| self.nav(ui));
        }
        egui::CentralPanel::default().show(ctx, |ui| self.mail_list(ui));

        if self.show_add_account {
            self.add_account_window(ctx);
        }
    }
}

// ----- helpers --------------------------------------------------------------

const TITLEBAR_HEIGHT: f32 = 30.0;
const TITLEBAR_BUTTON_WIDTH: f32 = 42.0;

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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

fn display_date(m: &MailMessage) -> String {
    m.timestamp
        .map(format_timestamp)
        .unwrap_or_else(|| m.date.clone())
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

/// egui's default fonts only cover Latin/Cyrillic/Greek; mail often needs
/// more. Best effort: if a system CJK font exists, use it as a fallback for
/// both proportional and monospace text.
fn install_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-sans-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
        "/usr/share/fonts/wenquanyi/wqy-zenhei/wqy-zenhei.ttc",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("jimao_cjk".into(), egui::FontData::from_owned(bytes).into());
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push("jimao_cjk".into());
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_sanitizes_names() {
        assert_eq!(slug("Gmail 主力"), "gmail");
        assert_eq!(slug("  Work! Mail "), "work-mail");
        assert_eq!(slug("???"), "account");
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate("abc", 5), "abc");
        let t = truncate("汉字汉字汉字", 2);
        assert_eq!(t, "汉字…");
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
                attachment_names: vec![],
            },
        ];
        sort_messages(&mut msgs);
        assert_eq!(msgs[0].uid, "2");
    }
}
