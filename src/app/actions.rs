//! Account and mail actions: selecting a folder or account, opening mail,
//! deleting, exporting and flagging, label changes, sync-all/sync-account, and
//! the action a row or context menu asks for.

use super::*;

impl KimoApp {
    pub(super) fn select_folder(&mut self, account_id: String, folder: String, protocol: Protocol) {
        self.selected_account = Some(account_id.clone());
        self.selected_folder = Some(folder.clone());
        self.selected_message = None;
        self.body_menu = None;
        self.body_menu_open = false;
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
            // A protocol with folders fetches the one clicked; a drop has only
            // INBOX, so opening it is the account's sync.
            let job = if crate::provider::capabilities(protocol).folders {
                Job::SyncFolder { account, folder }
            } else {
                Job::Sync {
                    account,
                    purge: false,
                    folder: None,
                    save: false,
                }
            };
            sync::spawn(job, self.ctx.clone(), self.tx.clone());
        }
    }
    pub(super) fn open_message(&mut self, msg: &MailMessage) {
        self.selected_message = Some(msg.uid.clone());
        // Opening a mail is a click elsewhere: a body menu from the mail that
        // was there before is not about this one.
        self.body_menu = None;
        self.body_menu_open = false;
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

        // Reading a mail in the Inbox is what the tray's badge stops saying:
        // publish that now rather than on a frame that may never come — the
        // click itself asked for no more than this one.
        self.update_tray_badge();

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
    pub(super) fn delete_account(&mut self, account: Account) {
        let account_id = account.id.clone();
        self.accounts.retain(|a| a.id != account_id);
        self.states.remove(&account_id);
        // Whatever was checked of it is not on screen any more.
        self.cancel_select();
        if self.selected_account.as_deref() == Some(account_id.as_str()) {
            self.selected_account = self.accounts.first().map(|a| a.id.clone());
            self.selected_folder = Some("INBOX".to_string());
            self.selected_message = None;
        }
        self.account_error = None;
        // The account row, and through its cascades every message of it, goes
        // on a worker thread; the UI has already dropped its own copy. A
        // failure comes back as `Event::AccountRemoveFailed`, which puts the
        // account back — the row survived, so the list has to say so.
        sync::spawn(
            Job::DeleteAccount { account },
            self.ctx.clone(),
            self.tx.clone(),
        );
        self.forget_blitz_pages(&account_id);
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
    pub(super) fn export_selected_message(&mut self) {
        // The reading pane's own lookup, like Delete's: a uid the pane no
        // longer shows is no mail to export, and the mail's subject is what
        // names the file.
        if let Some(key) = self.selected_mail_key() {
            self.export_mail(&key);
        }
    }
    /// Export one mail by where it lives — the title bar's cell for the mail
    /// the pane shows, and a row's context menu for the row it was opened on.
    /// The subject is looked up from the app's copy because it is what names
    /// the file; the original itself is fetched by the worker.
    pub(super) fn export_mail(&mut self, key: &MailKey) {
        let Some(msg) = self.mail_by_key(key) else {
            return;
        };
        let Some(account) = self
            .accounts
            .iter()
            .find(|account| account.id == key.account)
            .cloned()
        else {
            return;
        };
        sync::spawn(
            Job::ExportMessage {
                account,
                folder: key.folder.clone(),
                uid: key.uid.clone(),
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
    pub(super) fn toggle_flag_on(&mut self, key: &MailKey) {
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
    pub(super) fn toggle_flag(&mut self) {
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
    pub(super) fn delete_selected_message(&mut self) {
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
    pub(super) fn delete_mail(&mut self, account_id: &str, folder: &str, uid: &str) {
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
    /// Do what a message row's context menu asked for, for the mail the menu
    /// was opened on. The actions are the title bar's own — the same jobs, the
    /// same local-first flag — and Open is the one a row adds: it is the same
    /// path a left click takes, so a search hit or a label's mail switches the
    /// column to where it lives.
    pub(super) fn apply_mail_menu(&mut self, key: MailKey, action: MailMenuAction) {
        match action {
            MailMenuAction::Open => self.open_mail_at(&key.account, &key.folder, &key.uid),
            MailMenuAction::Reply => self.compose_about_key(MailAction::Reply, &key),
            MailMenuAction::ReplyAll => self.compose_about_key(MailAction::ReplyAll, &key),
            MailMenuAction::Forward => self.compose_about_key(MailAction::Forward, &key),
            MailMenuAction::Flag => self.toggle_flag_on(&key),
            MailMenuAction::Export => self.export_mail(&key),
            MailMenuAction::Delete => self.delete_mail(&key.account, &key.folder, &key.uid),
        }
    }
    /// Delete every checked mail. The selection is grouped by the folder the
    /// mails live in — a search can check hits from several accounts at once —
    /// and each group is one job on one connection (`delete_batches`).
    ///
    /// Nothing is cleared here: each row leaves the list when
    /// `Event::MessageDeleted` says the server agreed, and a mail the server
    /// refused keeps both its copies and its check, so the button is there to be
    /// pressed again.
    pub(super) fn delete_checked_messages(&mut self) {
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
    /// The list's model, brought up to date when anything it is derived from
    /// has moved: the folder the pane shows and its account, that folder's mail
    /// (`AccountUiState::folders_rev`), the threading switch, and the
    /// conversations the user opened.
    ///
    /// Grouping a folder is a map, a union-find and a sort over all of its
    /// mail, and the list is drawn on every frame — which is why the answer is
    /// kept rather than rebuilt (see [`ListRows`]).
    pub(super) fn refresh_list_rows(&mut self) {
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
    pub(super) fn my_addresses(&self) -> Vec<String> {
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
    pub(super) fn refresh_label_view(&mut self) {
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
    pub(super) fn toggle_label(&mut self, name: &str) {
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
    pub(super) fn end_search(&mut self) {
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
    pub(super) fn pane_mail_key(&self, uid: &str) -> Option<MailKey> {
        let account = self.selected_account.as_deref()?;
        let folder = self.selected_folder.as_deref()?;
        Some(MailKey::new(account, folder, uid))
    }
    /// The key of the mail the title bar's mail actions are about, if the pane
    /// shows one — the pane's own lookup, so a uid it no longer shows is no
    /// mail.
    pub(super) fn selected_mail_key(&self) -> Option<MailKey> {
        if !self.has_selected_message() {
            return None;
        }
        self.pane_mail_key(self.selected_message.as_deref()?)
    }
    /// One mail by where it lives: the lookup a context menu's actions use
    /// when the row they were opened on is not the pane's mail, and the one
    /// the file name of an export comes from.
    pub(super) fn mail_by_key(&self, key: &MailKey) -> Option<MailMessage> {
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
            .cloned()
    }
    /// The labels one mail carries, by name, read from the app's copy of the
    /// mail — which is what the popup's checklist is drawn from and what the
    /// card's chips say.
    pub(super) fn mail_labels(&self, key: &MailKey) -> Vec<String> {
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
    pub(super) fn set_label_on_mail(&mut self, key: &MailKey, name: &str, on: bool) {
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
    pub(super) fn set_flag_on_mail(&mut self, key: &MailKey, flagged: bool) {
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
}
