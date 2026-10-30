//! Worker events and everything that reacts to them: the frame's event drain,
//! sync scheduling and its clock, notifications, the tray badge, and the
//! hidden-window pump.

use super::*;

/// The notification one finished export raises: what happened, and where the
/// file went — the one thing about an export that is not known until the worker
/// has written it, since the dialog names the file and a session with no dialog
/// gets a name the writer numbered. `(summary, body)`, the two lines a desktop
/// notification shows, the way a batch of new mail's are (`new_mail_notices`).
///
/// An export is announced by the desktop and not by the line above the message
/// list (`sent_notice`, `AttachmentDownloaded`): its click was in the title
/// bar, and the file it wrote is not in the list.
pub(super) fn exported_notification(path: &Path) -> (String, String) {
    (
        "Mail exported".to_string(),
        format!("Saved to {}", path.display()),
    )
}
/// How many new mails one batch announces one by one. Past this a single
/// notification counts them instead: a folder that catches up with thirty
/// arrivals must not stack thirty popups on the desktop.
pub(super) const NOTIFY_MAX: usize = 3;
/// The notifications one batch of new mail raises: the mail itself when there
/// are few, one line counting them when there are many. `(summary, body)`, the
/// two lines a desktop notification shows.
pub(super) fn new_mail_notices(folder: &str, arrivals: &[Arrival]) -> Vec<(String, String)> {
    if arrivals.len() <= NOTIFY_MAX {
        return arrivals
            .iter()
            .map(|arrival| (arrival.from.clone(), arrival.subject.clone()))
            .collect();
    }
    let body = arrivals
        .iter()
        .take(NOTIFY_MAX)
        .map(|arrival| format!("{}: {}", arrival.from, arrival.subject))
        .collect::<Vec<_>>()
        .join("\n");
    vec![(format!("{} new messages in {folder}", arrivals.len()), body)]
}
/// Whether new mail filed in a folder of this kind is announced. Not the
/// folders the user's own copies live in (Sent, Drafts) and not the ones mail
/// is filtered into rather than delivered to (Trash, Junk) — nor Gmail's All
/// Mail and Starred, where every mail the account holds turns up a second
/// time, so announcing them would double every arrival.
pub(super) fn announces_new_mail(kind: FolderKind) -> bool {
    !matches!(
        kind,
        FolderKind::Sent
            | FolderKind::Drafts
            | FolderKind::Trash
            | FolderKind::Junk
            | FolderKind::AllMail
            | FolderKind::Starred
    )
}
/// How long the line above the message list stays there: long enough to read
/// where a downloaded file went — or that a mail went out — and act on it, short
/// enough not to become furniture. Whatever it says does not change when the line
/// goes.
pub(super) const NOTICE_TIME: Duration = Duration::from_secs(8);
/// The automatic sync (Preferences → General): how many minutes apart its sweeps
/// are unless the preference says otherwise, and the range the preference
/// offers. A minute is the shortest that is not a busy loop, and a day the
/// longest that still reads as "automatically" rather than as a schedule.
pub(super) const AUTO_SYNC_MINUTES: u32 = 15;
pub(super) const AUTO_SYNC_MIN_MINUTES: u32 = 1;
pub(super) const AUTO_SYNC_MAX_MINUTES: u32 = 1440;
pub(super) const SECS_PER_MINUTE: u64 = 60;
/// How long between automatic syncs, from the preference's minutes: the app's
/// own default when settings.json names none, and clamped to the range the
/// preferences page offers however the file was edited.
pub(super) fn auto_sync_interval(minutes: Option<u32>) -> Duration {
    let minutes = minutes
        .unwrap_or(AUTO_SYNC_MINUTES)
        .clamp(AUTO_SYNC_MIN_MINUTES, AUTO_SYNC_MAX_MINUTES);
    Duration::from_secs(minutes as u64 * SECS_PER_MINUTE)
}
/// How long until the automatic sync is next due: `None` when it is not running
/// at all — switched off in Preferences — and zero from the moment the interval
/// since the last sweep has passed, however long ago that was.
pub(super) fn auto_sync_wait(
    enabled: bool,
    last: Instant,
    every: Duration,
    now: Instant,
) -> Option<Duration> {
    enabled.then(|| every.saturating_sub(now.saturating_duration_since(last)))
}

impl KimoApp {
    pub(super) fn drain_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            self.apply_event(event);
        }
    }
    /// Whether a close should hide the window instead of quitting. The loop
    /// asks before it honours a compositor close (`Kimo::window_event`).
    pub(crate) fn close_to_tray(&self) -> bool {
        self.close_to_tray
    }
    /// The work that has to continue while the window is hidden: worker
    /// results are drained (a notification is one of them) and the automatic
    /// sync clock keeps turning. The UI itself is not run, so a hidden Kimo
    /// costs no frames.
    pub(crate) fn pump_hidden(&mut self) {
        let ctx = self.ctx.clone();
        self.drain_events();
        // A hidden window still hears the mail arrive, and still hears it read
        // elsewhere: the badge is as right without frames as with them.
        self.update_tray_badge();
        // egui's own repaint request may be suppressed while no window is
        // drawing, so the clock is put on the event loop's wake channel too.
        if let Some(wait) = self.pump_auto_sync(&ctx) {
            crate::wake::after(wait);
        }
    }
    pub(super) fn apply_event(&mut self, event: Event) {
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
                    // A `LIST` that names the mailboxes the column already
                    // shows — a warm sync — is not a change: the rows stay,
                    // and nothing is derived again.
                    if state.folders != folders {
                        state.folders = folders;
                        state.folders_changed();
                    }
                }
            }
            Event::OlderMail {
                account_id,
                folder,
                available,
            } => {
                // What the server said about older mail — the one thing the
                // list's button is drawn from. Nothing else on screen depends
                // on it (the column counts mail, not marks), so no rows are
                // derived again for it.
                if let Some(state) = self.states.get_mut(&account_id) {
                    if let Some(folder) = state
                        .mailbox
                        .folders
                        .iter_mut()
                        .find(|candidate| candidate.name == folder)
                    {
                        folder.can_load_older = available;
                    }
                }
            }
            Event::Messages {
                account_id,
                folder,
                uidvalidity,
                messages,
                new_mail,
            } => {
                // The worker has already stored this batch; what follows is
                // the in-memory copy the list and the reading pane draw from.
                // Whether the batch moved the folder onto a new UIDVALIDITY
                // generation is remembered for the selection below: the two
                // cannot be touched in the same borrow of the state.
                let mut rotated = false;
                // The mail of a batch the fetch found beyond the folder's mark
                // — the news a notification may be raised from. Collected here
                // and announced below, because the state is borrowed until
                // then.
                let mut arrivals: Vec<Arrival> = Vec::new();
                if let Some(state) = self.states.get_mut(&account_id) {
                    if state.mailbox.folders.iter().all(|f| f.name != folder) {
                        state.mailbox.folders.push(storage::Folder {
                            name: folder.clone(),
                            uidvalidity: None,
                            synced: false,
                            // Mail was just fetched for it, so the server has
                            // this mailbox, whatever the last `LIST` said: it
                            // is one of the folders the column lists rather
                            // than one kept for its cached mail.
                            listed: true,
                            // The job that fetched the mail reports what the
                            // server said about older mail in `OlderMail`,
                            // which is what sets this for real.
                            can_load_older: false,
                            messages: Vec::new(),
                        });
                    }
                    if !state.folders.iter().any(|name| name == &folder) {
                        state.folders.push(folder.clone());
                    }
                    let fc = state
                        .mailbox
                        .folders
                        .iter_mut()
                        .find(|f| f.name == folder)
                        .expect("folder just inserted");
                    if let Some(uv) = uidvalidity {
                        if fc.uidvalidity.is_some() && fc.uidvalidity != Some(uv) {
                            fc.messages.clear(); // new UIDVALIDITY generation: old UIDs are meaningless
                            rotated = true;
                        }
                        fc.uidvalidity = Some(uv);
                    }
                    for m in messages {
                        match fc.messages.iter().position(|x| x.uid == m.uid) {
                            Some(i) => {
                                let keep_seen = fc.messages[i].seen || m.seen;
                                let mut merged = m;
                                merged.seen = keep_seen;
                                // Labels are the user's own marks and were
                                // never in the fetched mail, so the copy the
                                // app holds is the one that stays — a POP3
                                // re-sync re-downloads every message it has.
                                merged.labels = std::mem::take(&mut fc.messages[i].labels);
                                fc.messages[i] = merged;
                            }
                            None => {
                                // Mail that is *news*: what the fetch found
                                // beyond the folder's mark, still unread, and
                                // not already here under this folder's own uid.
                                if new_mail && !m.seen {
                                    arrivals.push(Arrival::of(&m));
                                }
                                fc.messages.push(m);
                            }
                        }
                    }
                    sort_messages(&mut fc.messages);
                    fc.synced = true;
                    // Mail was just fetched for it, so the server has this
                    // mailbox — the `stored` copy of the flag says the same
                    // thing, and the two have to agree across a restart.
                    fc.listed = true;
                    // New mail, and with it a count the column shows.
                    state.folders_changed();
                }
                // The UIDs of the old generation are meaningless, and so is a
                // check made of one: the select-all box counts the selection
                // against the list, so it must never hold a mail the list no
                // longer has.
                if rotated {
                    self.selected_mails
                        .retain(|key| !(key.account == account_id && key.folder == folder));
                }
                self.announce_new_mail(&account_id, &folder, arrivals);
            }
            Event::SyncDone { account_id } => {
                if let Some(state) = self.states.get_mut(&account_id) {
                    state.syncing = false;
                }
            }
            Event::SyncFailed { account_id, error } => {
                if let Some(state) = self.states.get_mut(&account_id) {
                    state.syncing = false;
                    state.error = Some(error);
                }
            }
            Event::PluginAuth { account_id, error } => {
                // The form waited on this account; it is the first place the
                // answer belongs, whether it reopens for the error or closes
                // for the tokens.
                if self.plugin_signing_in.as_deref() == Some(account_id.as_str()) {
                    self.plugin_signing_in = None;
                    let waiting_on_it = self.show_account_form
                        && self.account_form.editing.as_deref() == Some(account_id.as_str());
                    match (&error, waiting_on_it) {
                        // A failed sign-in puts the form back on the account
                        // with the reason beside the button that was clicked.
                        (Some(_), _) => {
                            if let Some(account) =
                                self.accounts.iter().find(|a| a.id == account_id).cloned()
                            {
                                self.account_form = AccountForm::edit(&account);
                                self.show_account_form = true;
                            }
                        }
                        (None, true) => self.show_account_form = false,
                        (None, false) => {}
                    }
                }
                match error {
                    Some(error) => self.account_error = Some(error),
                    None => {
                        self.account_error = None;
                        // Tokens are in the row: take them into the window's
                        // copy and sync the mailbox the sign-in just opened.
                        if let Ok(accounts) = crate::db::load_accounts() {
                            if let Some(fresh) = accounts
                                .into_iter()
                                .find(|account| account.id == account_id)
                            {
                                if let Some(account) = self
                                    .accounts
                                    .iter_mut()
                                    .find(|account| account.id == account_id)
                                {
                                    *account = fresh.clone();
                                }
                                sync::spawn(
                                    Job::Sync {
                                        account: fresh,
                                        purge: false,
                                        folder: None,
                                        save: false,
                                    },
                                    self.ctx.clone(),
                                    self.tx.clone(),
                                );
                            }
                        }
                    }
                }
            }
            Event::AccountRemoveFailed { account, error } => {
                // The row is still in the database, so the next start would
                // load the account back. Put it back now instead, and say why:
                // a click that silently undid itself is worse than a click
                // that failed loudly.
                self.account_error = Some(format!("Could not remove {}: {error}", account.name));
                if !self.accounts.iter().any(|a| a.id == account.id) {
                    self.accounts.push(account);
                }
            }
            Event::MessageDeleted {
                account_id,
                folder,
                uid,
            } => {
                // The worker has already taken the mail off the server and
                // out of the database; what is left is this in-memory copy.
                //
                // A checked mail that went is not checked any more: its row is
                // about to leave the list, and the selection must not keep a
                // key nothing on screen stands for.
                self.selected_mails
                    .remove(&MailKey::new(&account_id, &folder, &uid));
                if let Some(state) = self.states.get_mut(&account_id) {
                    if let Some(fc) = state.mailbox.folders.iter_mut().find(|f| f.name == folder) {
                        fc.messages.retain(|m| m.uid != uid);
                    }
                    // A mail fewer, and one the folder's count was made of.
                    state.folders_changed();
                }
                if self.selected_account.as_deref() == Some(account_id.as_str())
                    && self.selected_folder.as_deref() == Some(folder.as_str())
                    && self.selected_message.as_deref() == Some(uid.as_str())
                {
                    self.selected_message = None;
                }
                self.forget_blitz_message(&account_id, &folder, &uid);
                // The results are a snapshot of the database from when the
                // query ran, so they have to be asked for again for the
                // deleted mail to leave them too.
                if self.search.is_active() {
                    self.spawn_search();
                }
            }
            // An exported mail is announced by the desktop, not by a line above
            // the message list: the click was in the title bar and the file is
            // not in the list, so the notification is where the answer belongs —
            // and where the file went is the one thing about an export that is
            // not known until the worker has written it.
            //
            // The notification preference (Preferences → General) is not
            // consulted: it is a switch on mail that arrives by itself, and this
            // answers a click the user has just made. A click answered with
            // silence would be worse than one answered where they are not
            // looking.
            Event::MessageExported { path } => {
                let (summary, body) = exported_notification(&path);
                crate::notify::show(&summary, &body, self.ctx.pixels_per_point());
            }
            // A downloaded attachment keeps the line: that click was a card under
            // the mail, and the line is drawn beside what was clicked. Where the
            // file went is the one thing the app could not know when the work was
            // asked for, since a name that was taken is numbered.
            Event::AttachmentDownloaded { path } => {
                self.notice = Some((format!("Saved to {}", path.display()), Instant::now()));
            }
            Event::FilesChosen { files } => {
                // The dialog came back: the card is the one that asked, so the
                // files go on whatever draft is open — and nowhere if the card
                // was closed while the dialog was up, because the draft it was
                // asking for is gone with it.
                self.picking_files = false;
                for path in files {
                    if let Some(draft) = self.compose.as_mut() {
                        attach_file(draft, path);
                    }
                }
                // Empty is the dialog cancelled: no error, and nothing to say.
                self.ctx.request_repaint();
            }
            Event::FileChoiceFailed { error } => {
                self.picking_files = false;
                if let Some(draft) = self.compose.as_mut() {
                    draft.attach_error = Some(error);
                }
                self.ctx.request_repaint();
            }
            Event::MessageSent {
                recipients, copy, ..
            } => {
                // The server has the mail, so the card has nothing left to
                // hold: the draft goes with it, and the one line above the
                // message list says where the mail went.
                self.compose = None;
                self.notice = Some((sent_notice(&recipients, &copy), Instant::now()));
            }
            Event::MessageSendFailed { error, .. } => {
                // The draft is still the only copy of the mail, so the card
                // stays open with the server's own words in it — "550 5.1.1 no
                // such user" is the whole answer to why nothing went out — and
                // Send is enabled again.
                if let Some(draft) = self.compose.as_mut() {
                    draft.sending = false;
                    draft.error = Some(error);
                }
            }
            Event::ImageLoaded {
                url,
                width,
                height,
                rgba,
            } => {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [width as usize, height as usize],
                    &rgba,
                );
                let texture = self.ctx.load_texture(&url, image, Default::default());
                self.images.insert(url, ImageStatus::Ready(texture));
            }
            Event::ImageFailed { url } => {
                self.images.insert(url, ImageStatus::Failed);
            }
            Event::StylesheetLoaded { url, text } => {
                self.stylesheets.insert(url, Some(text));
            }
            Event::StylesheetFailed { url } => {
                self.stylesheets.insert(url, None);
            }
            Event::HtmlRendered { key, page } => self.store_blitz_page(key, page),
            Event::HtmlRenderFailed { key, error } => {
                if let Some(slot) = self.blitz_pages.get_mut(&key) {
                    *slot = BlitzPage::Failed(error);
                }
            }
            Event::SearchResults { seq, hits } => {
                // A result for a query the user has moved on from never
                // reaches the screen.
                if seq == self.search_seq {
                    self.search_hits = hits;
                    self.search_running = false;
                    self.search_error = None;
                    // A fresh answer is the whole of what the column shows, so
                    // a checked hit this one no longer names is not on screen
                    // any more — and the select-all box counts the selection
                    // against the hits, so it must not be able to count a mail
                    // that is not one. Only while a search is what the column
                    // shows: an answer for a query already cleared says nothing
                    // about a folder's selection.
                    if self.search.is_active() {
                        let live: HashSet<(&str, &str, &str)> = self
                            .search_hits
                            .iter()
                            .map(|hit| {
                                (
                                    hit.account_id.as_str(),
                                    hit.folder.as_str(),
                                    hit.uid.as_str(),
                                )
                            })
                            .collect();
                        self.selected_mails.retain(|key| {
                            live.contains(&(
                                key.account.as_str(),
                                key.folder.as_str(),
                                key.uid.as_str(),
                            ))
                        });
                    }
                }
            }
            Event::SearchFailed { seq, error } => {
                if seq == self.search_seq {
                    self.search_running = false;
                    self.search_error = Some(error);
                }
            }
            Event::Gpus(scan) => {
                self.gpu_scanning = false;
                self.gpu_scan = Some(scan);
            }
            Event::FontsScanned(families) => {
                self.font_families = Some(families);
                // settings.json may name families that only now resolve.
                self.apply_fonts();
            }
        }
    }
    /// Raise the desktop notifications for one batch of arrived mail: one per
    /// mail while there are few of them, one counting them when there are many
    /// (`new_mail_notices`). Nothing is raised when the preference is off, when
    /// the folder is one whose new mail is not announced (`announces_new_mail`),
    /// or for a mail the account already holds under another folder.
    ///
    /// The work is `crate::notify`'s and it never happens here: `show` only posts
    /// to the thread that talks to the notification daemon, so neither this
    /// frame nor a sync job ever waits for one. The tray's badge is not raised
    /// here: it follows the Inbox's unread mail rather than the fetch
    /// (`update_tray_badge`).
    pub(super) fn announce_new_mail(&self, account_id: &str, folder: &str, arrivals: Vec<Arrival>) {
        if arrivals.is_empty() {
            return;
        }
        // The wire form is what the state and the database key a folder by;
        // what is announced, and what the folder is recognized as, is the
        // name a person reads.
        let display = crate::protocol::imap::decode_mailbox_name(folder);
        if !announces_new_mail(folders::kind_of(&display)) {
            return;
        }
        let elsewhere = self.message_ids_elsewhere(account_id, folder);
        let arrivals: Vec<Arrival> = arrivals
            .into_iter()
            .filter(|arrival| !elsewhere.contains(arrival.message_id.as_str()))
            .collect();
        if arrivals.is_empty() {
            return;
        }
        if !self.notifications {
            return;
        }
        let scale = self.ctx.pixels_per_point();
        for (summary, body) in new_mail_notices(&display, &arrivals) {
            crate::notify::show(&summary, &body, scale);
        }
    }
    /// Publish the tray's unread badge: up while any account's Inbox holds
    /// mail nobody has read, wherever the window is and whether or not there is
    /// one. The badge is the mail's own state rather than an event, which is
    /// what makes it right on the first frame of a session — the database
    /// loaded the marks — and right again when a sync reports a mail read
    /// somewhere else.
    ///
    /// The flag the tray thread polls is written only when the state changes:
    /// this runs every frame, and a frame that changed nothing says nothing.
    pub(super) fn update_tray_badge(&mut self) {
        let unread = self.inbox_unread();
        if unread != self.tray_badge {
            self.tray_badge = unread;
            crate::tray::set_unread(unread);
        }
    }
    /// Whether any account's Inbox holds unread mail. The unread counts are the
    /// ones the navigation column already keeps per folder (`FolderRows`),
    /// derived when an account's mail changed and reused until it does again —
    /// so asking this every frame walks the folders, never the mail.
    pub(super) fn inbox_unread(&mut self) -> bool {
        self.states.values_mut().any(|state| {
            let rows = state.nav();
            rows.listed
                .iter()
                .chain(rows.unlisted.iter())
                .any(|row| matches!(row.kind, FolderKind::Inbox) && row.unread > 0)
        })
    }
    /// The `Message-ID`s the account holds under any folder but this one — the
    /// one question that keeps a message its server files twice (Gmail's All
    /// Mail, a label beside the Inbox) from being announced twice. Gathered in
    /// one pass rather than asked per mail: a batch can hold hundreds of mails,
    /// and every answer is about the same folders. Mail cached before Kimo kept
    /// that header has no id, is not in the set, and is announced.
    pub(super) fn message_ids_elsewhere<'a>(
        &'a self,
        account_id: &str,
        folder: &str,
    ) -> HashSet<&'a str> {
        self.states
            .get(account_id)
            .map(|state| {
                state
                    .mailbox
                    .folders
                    .iter()
                    .filter(|other| other.name != folder)
                    .flat_map(|other| other.messages.iter())
                    .map(|mail| mail.message_id.as_str())
                    .filter(|id| !id.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }
    /// One account's folder list and new mail — the sync cell on its row in
    /// the navigation column. The same job the title bar's sweep gives every
    /// account, with the one difference the cell is about: only this account
    /// is asked. A folder that has never been fetched is included when it is
    /// the one the window is showing, because that folder is never part of the
    /// sweep's queue until it has a mark.
    pub(super) fn sync_account(&mut self, account_id: &str) {
        // A sync already in flight is left alone: the row draws its spinner
        // while it runs, and a second job would fetch the same mail twice.
        if self
            .states
            .get(account_id)
            .is_some_and(|state| state.syncing)
        {
            return;
        }
        let Some(account) = self
            .accounts
            .iter()
            .find(|account| account.id == account_id)
            .cloned()
        else {
            return;
        };
        let open = match (&self.selected_account, &self.selected_folder) {
            (Some(selected), Some(folder))
                if selected == account_id && !self.selected_folder_synced() =>
            {
                Some(folder.clone())
            }
            _ => None,
        };
        sync::spawn(
            Job::Sync {
                account,
                purge: false,
                folder: open,
                save: false,
            },
            self.ctx.clone(),
            self.tx.clone(),
        );
    }
    /// Every account's folder list and new mail — and, for the account whose
    /// folder is open, that folder's *first* fetch when it has none. The column
    /// says "Not fetched yet — click Sync in the title bar" for exactly that
    /// case, so the button has to honour it: the sweep only revisits folders
    /// that already carry a mark.
    pub(super) fn sync_all(&mut self) {
        // The automatic sync's clock is measured from the last sweep, whoever
        // asked for it: pressing Sync is not a reason to be swept again a moment
        // later.
        self.auto_synced_at = Instant::now();
        for account in self.accounts.clone() {
            let open = match (&self.selected_account, &self.selected_folder) {
                (Some(selected), Some(folder))
                    if *selected == account.id && !self.selected_folder_synced() =>
                {
                    Some(folder.clone())
                }
                _ => None,
            };
            sync::spawn(
                Job::Sync {
                    account,
                    purge: false,
                    folder: open,
                    save: false,
                },
                self.ctx.clone(),
                self.tx.clone(),
            );
        }
    }
    /// The automatic sync: every `auto_sync_every`, and only while the
    /// preference is on — it is the one sync nobody asks for, and it is off
    /// unless it was asked for once. Nothing else asks for a frame minutes from
    /// now, so the wait is what keeps the clock ticking — a frame with nothing
    /// to draw is not a frame the app would otherwise be given.
    ///
    /// A sweep that is already in flight is left to finish (a second one would
    /// fetch the same mail twice); the clock is set either way, so a busy
    /// account is asked again a full interval later rather than every frame.
    ///
    /// The wait is returned so a hidden app can also put it on the event
    /// loop's wake channel: there is no frame to ask.
    pub(super) fn pump_auto_sync(&mut self, ctx: &egui::Context) -> Option<Duration> {
        let now = Instant::now();
        let wait = auto_sync_wait(
            self.auto_sync,
            self.auto_synced_at,
            self.auto_sync_every,
            now,
        )?;
        if wait.is_zero() {
            self.auto_synced_at = now;
            if !self.sync_in_flight() {
                self.sync_all();
            }
        }
        ctx.request_repaint_after(wait);
        Some(wait)
    }
    /// Whether any account is syncing right now, which is what the automatic
    /// sync waits for.
    pub(super) fn sync_in_flight(&self) -> bool {
        self.states.values().any(|state| state.syncing)
    }
}
