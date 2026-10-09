//! The egui application: all UI state and drawing. Network results arrive as
//! `sync::Event`s drained at the top of every frame; opening a socket happens
//! only in worker threads.

use std::collections::HashMap;
use std::sync::mpsc;

use crate::models::{Account, MailMessage, Protocol, TlsMode};
use crate::storage::{self, FolderCache};
use crate::sync::{self, Event, Job};

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
}

#[derive(Default)]
pub struct AccountUiState {
    pub folders: Vec<String>,
    pub cache: storage::AccountCache,
    pub syncing: bool,
    pub first_sync_done: bool,
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
                    state.first_sync_done = true;
                    snapshot = Some(state.cache.clone());
                }
                if let Some(cache) = snapshot {
                    let _ = storage::save_cache(&account_id, &cache);
                }
            }
            Event::SyncFailed { account_id, error } => {
                if let Some(state) = self.states.get_mut(&account_id) {
                    state.syncing = false;
                    state.first_sync_done = true;
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

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("⟳ Sync").clicked() {
                self.sync_all();
            }
            if ui.button("＋ Add account").clicked() {
                self.add_form = AddAccountForm::default();
                self.show_add_account = true;
            }
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("Filter by subject or sender…")
                    .desired_width(220.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                for account in self.accounts.clone() {
                    let Some(state) = self.states.get(&account.id) else {
                        continue;
                    };
                    if state.syncing {
                        ui.spinner()
                            .on_hover_text(format!("{}: syncing…", account.name));
                    } else if let Some(err) = &state.error {
                        ui.label(egui::RichText::new("⚠").color(egui::Color32::RED))
                            .on_hover_text(format!("{}: {err}", account.name));
                    }
                }
            });
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.accounts.is_empty() {
                    ui.label("No accounts configured.");
                    ui.label(
                        "Use \"＋ Add account\" in the toolbar to add an IMAP or POP3 mailbox.",
                    );
                    return;
                }
                for account in self.accounts.clone() {
                    let (folders, syncing, error) = {
                        let state = self.states.entry(account.id.clone()).or_default();
                        (state.folders.clone(), state.syncing, state.error.clone())
                    };

                    ui.horizontal(|ui| {
                        ui.strong(account.name.as_str());
                        ui.label(egui::RichText::new(account.protocol.label()).small().weak());
                        if syncing {
                            ui.spinner();
                        }
                        if ui.button("⟳").on_hover_text("Sync this account").clicked() {
                            sync::spawn(
                                Job::Sync {
                                    account: account.clone(),
                                },
                                self.ctx.clone(),
                                self.tx.clone(),
                            );
                        }
                        let confirming =
                            self.confirm_delete.as_deref() == Some(account.id.as_str());
                        let label = if confirming { "Remove?" } else { "✕" };
                        let button = if confirming {
                            egui::Button::new(egui::RichText::new(label).color(egui::Color32::RED))
                                .small()
                        } else {
                            egui::Button::new(label).small()
                        };
                        if ui
                            .add(button)
                            .on_hover_text("Remove account (and its local cache)")
                            .clicked()
                        {
                            if confirming {
                                self.delete_account(&account.id);
                                self.confirm_delete = None;
                            } else {
                                self.confirm_delete = Some(account.id.clone());
                            }
                        }
                    });
                    if let Some(err) = error {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                        ui.label(
                            egui::RichText::new(format!("⚠ {err}"))
                                .small()
                                .color(egui::Color32::RED),
                        );
                    }

                    let folder_list = if folders.is_empty() {
                        vec!["INBOX".to_string()]
                    } else {
                        folders
                    };
                    for folder in folder_list {
                        let selected = self.selected_account.as_deref()
                            == Some(account.id.as_str())
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
                    ui.add_space(6.0);
                }
            });
    }

    fn message_list(&mut self, ui: &mut egui::Ui) {
        if self.accounts.is_empty() || self.selected_account.is_none() {
            ui.centered_and_justified(|ui| {
                ui.heading("Jimao");
            });
            return;
        }
        let messages = self.current_messages();
        if messages.is_empty() {
            ui.centered_and_justified(|ui| {
                let text = if !self.filter.is_empty() {
                    "No messages match the filter."
                } else if self.selected_folder_synced() {
                    "This folder is empty."
                } else {
                    "Not fetched yet — click ⟳ Sync."
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
                                    egui::RichText::new(truncate(&msg.from, 30)),
                                    egui::RichText::new(truncate(&msg.subject, 70)),
                                )
                            } else {
                                (
                                    egui::RichText::new(truncate(&msg.from, 30)).strong(),
                                    egui::RichText::new(truncate(&msg.subject, 70)).strong(),
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

    fn preview(&mut self, ui: &mut egui::Ui) {
        let Some(msg) = self.selected_message_view() else {
            return;
        };
        ui.vertical(|ui| {
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
                    ui.label(msg.body_text.as_str());
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

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| self.toolbar(ui));
        egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(230.0)
            .show(ctx, |ui| self.sidebar(ui));
        if self.selected_message.is_some() {
            egui::TopBottomPanel::bottom("preview")
                .resizable(true)
                .default_height(300.0)
                .show(ctx, |ui| self.preview(ui));
        }
        egui::CentralPanel::default().show(ctx, |ui| self.message_list(ui));

        if self.show_add_account {
            self.add_account_window(ctx);
        }
    }
}

// ----- helpers --------------------------------------------------------------

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
                attachment_names: vec![],
            },
        ];
        sort_messages(&mut msgs);
        assert_eq!(msgs[0].uid, "2");
    }
}
