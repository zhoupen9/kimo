# Storage — database, settings and caches

Read when touching `crates/kimo-store/src/db.rs`, `crates/kimo-core/src/storage.rs`, `crates/kimo-store/src/search.rs`, a schema migration, or a path under `~/.local/share/kimo`.

- **Persistence:** accounts and cached mail live in one SQLite database,
  `~/.local/share/kimo/mail.db` (mode 0600, WAL), with an FTS5 index over the
  mail. `~/.config/kimo/settings.json` is the only JSON left.

## Storage layout

- `~/.local/share/kimo/mail.db` — the one database (SQLite, WAL, mode 0600
  because it holds the passwords). `accounts` (id, name, protocol, host,
  port, username, password, tls, smtp_host, smtp_port, smtp_tls, smtp_username,
  smtp_password, position), `folders` (account_id, name,
  uidvalidity, uid_next, first_num, oldest_uid, synced, listed — the fetch
  marks described in [architecture.md](architecture.md), then `listed`, what the
  last `LIST` named),
  `messages` keyed by `(account_id, folder, uid)` with
  the subject, addresses, both bodies, the preview, the bigram column and the
  three headers a conversation is stitched from (`message_id`, `in_reply_to`,
  `refs` — the last one is not `references`: that is a SQL keyword),
  `attachments` (the name, the MIME type and the decoded size — never the
  bytes; see the attachment-card bullet in [ui-mail.md](ui-mail.md)) and
  `inline_images` (raw bytes,
  not base64) as children of a message, `labels` (a name, unique
  case-insensitively — a label is nothing else) and `message_labels` (which
  labels a message carries, cascading with it), and the external-content FTS5
  table `messages_fts`, which only the
  three triggers in `db::SCHEMA_SQL` ever write. `PRAGMA foreign_keys = ON`
  per connection and `ON DELETE CASCADE` all the way down, so
  `Job::DeleteAccount` is one `DELETE FROM accounts`. `PRAGMA user_version`
  is the schema version, walked one step at a time by `db::migrate`, and
  `SCHEMA_SQL` is the schema a *new* database is born with. Change the DDL and
  you change both: a database born before the change is brought forward by a new
  `if start < n` step, which is one transaction with the `user_version` write
  inside it — a crash rolls the whole step back instead of leaving a database
  half a step forward. The version-1 step is the shape to copy: it adds the sync
  marks and derives `uid_next` from the UIDs the database already holds
  (`derive_imap_marks`, sharing `DERIVE_UID_NEXT_SQL` with `save_messages`, so
  the two can never disagree), which is what keeps an upgrade from refetching a
  batch it has. The version-5 step is its opposite: it adds the five `smtp_*`
  columns and derives nothing, because a database from before it has no
  outgoing server to move over — every account it holds starts as one that only
  reads mail, and Preferences → Accounts is where it gets a way to send. The
  version-9 step is the flag: one `ALTER TABLE` adding `flagged` at the end of
  `messages` (where `SCHEMA_SQL` puts it too, so a migrated table and a fresh one
  have the same shape), defaulting to 0 — there were no flags before it and no
  way to have meant one, so nothing is derived. The
  version-6 step is that kind of step too: it adds an attachment's `content_type`
  and `size`, and derives nothing, because the part a mail arrived in is not
  stored — the rows it holds keep an empty type and a zero size, and only mail
  fetched after the step carries them. The version-7 step is the third of that
  kind: it adds the threading headers, which were never stored either, and a
  mail from before it threads by subject instead (`crate::threads`). The
  version-8 step is the fourth: it adds the two label tables and derives
  nothing, because a database from before it has no labels and no way to have
  meant one — every mail in it starts unlabelled. The version-10 step is the
  fifth of that kind: five nullable `accounts` columns (`provider`,
  `provider_config`, and the three OAuth token columns), because a database
  from before it has no plugin-backed account — every account it holds stays
  the built-in client its `protocol` names.

- The account list is **in the database, passwords in plain text**. That is a
  documented, accepted limitation; do not add a secrets daemon (rule 1). An
  account's `id` is minted once (the slug of its name, numbered if taken) and
  is never reused: it is the key of the account's row and of all of its mail,
  so renaming an account in Preferences keeps both. `position` is the
  navigation column's order (new accounts append, an edit keeps its place).
  Editing an account is an in-place update of the same id; its stored mail is
  purged only when the form's endpoint (protocol, host, port, username)
  changed, because only then can it describe somebody else's mailbox
  (`AccountForm::points_elsewhere` — the purge runs inside the same sync job,
  `Job::Sync { purge }`, so nothing can reorder those two steps). `save`, on
  that job, is what writes the account row, and only the form's job sets it:
  the row has to exist before the mail below can refer to it, and a sync that
  wrote it would put a *removed* account back the moment a job spawned before
  the removal reached the database — the sweep of a second window, or one
  queued just before the click — so the account would return at the next
  start. A removal that fails is answered rather than swallowed: the job sends
  `Event::AccountRemoveFailed` with the account, the window puts it back in
  the list, and Preferences → Accounts says why (`KimoApp::account_error`) —
  the row is still there, so hiding the account would be a lie the next start
  would correct. An account's
  outgoing server is `Account.smtp` (`None` is an account that cannot send,
  which is what leaves the compose card's Send disabled): five columns written
  together by `save_account_on` and read by `smtp_from_row`, where a NULL host
  is "no outgoing server" and a NULL username is "submit as the account
  itself" (`Account::smtp_login`; a row naming a host with no usable port or
  TLS mode fails the load the way an unknown protocol does). Editing the SMTP
  settings is *not* a change of mailbox — `points_elsewhere` ignores them — so
  moving an outgoing server never purges the cached mail.

- `~/.config/kimo/settings.json` — UI settings: the theme
  (`system`/`light`/`dark`; "system" resolves via GTK/KDE config files, no
  subprocesses or D-Bus), `html_renderer` (`egui`/`blitz`, absent means
  `egui`), `threaded` (whether the message list groups mail into
  conversations; absent means the app's own default, which is on), `auto_sync`
  and `auto_sync_minutes` (the automatic sync's switch and its interval, both
  absent by default — see [preferences.md](preferences.md)), `notifications`
  (whether new mail is announced on the desktop; absent means on — see
  [platform.md](platform.md)), `disabled_vendors` (the provider presets
  switched off in Preferences → Accounts, by key — `"gmail"`, `"outlook"`, …;
  absent means all on, an unknown key is ignored, and `custom` is never
  written — see [providers.md](providers.md)), `disabled_plugins` (installed
  plugin ids switched off in Preferences → Plugins; a disabled module is not
  compiled or instantiated at startup, and absent means every plugin is
  enabled — see [mail-spec.md](mail-spec.md)), and the font
  families picked in Preferences →
  Appearance (`ui_font`, `mono_font`, `cjk_font`; absent means egui's built-in
  font plus the automatic CJK fallback), and `gpu`, the PCI address of the
  graphics card Preferences → Graphics picked (`"0000:03:00.0"`; absent means
  the Vulkan loader's own choice, and a value that names no installed card is
  ignored — see [preferences.md](preferences.md)). Family *names* are stored,
  not paths, so the file survives font updates; a name that no longer resolves
  is simply ignored. Older files may
  still carry a `render_mode` field from the removed WebKitGTK renderer; it
  is ignored on load — do not reuse that name. The palette that the theme
  resolves to is *not* in this file; it is a compiled-in default — see
  [theme.md](theme.md).

- `~/.local/share/kimo/cache/notify/icon-<px>.png` — the brand icon a
  notification hands the daemon through its `image-path` hint, one file per
  pixel size (`48 · screen scale`, see [platform.md](platform.md)). It
  never changes once written, so a restart — or a daemon that still has the
  path loaded — reuses it; a session that cannot write it announces without an
  image rather than not at all.

- `~/.local/share/kimo/cache/resources/<sha1hex>.bin` — fetched remote
  mail resources, keyed by the SHA-1 of their URL. The rest of that directory
  is what the pre-database layout left behind: `<account-id>.json` mail
  caches and `~/.config/kimo/accounts.json`. `db::import_legacy_json` reads
  both once, at `user_version` 0, imports them through the normal upsert path
  and deletes each file it has read (one it cannot parse is left alone, and a
  cache file whose account id is unknown is not imported at all). Keep that
  code until no installation can still predate the database.

- The project was renamed from Jimao, so a pre-rename `jimao` directory under
  either XDG root is moved to the `kimo` name the first time those paths are
  used (`storage::migrate_legacy_dir`); an existing `kimo` directory wins and
  failures are ignored. Drop that helper and `LEGACY_APP_DIR` once no
  installation can still predate the rename.

## Search

- Searching is a job like any other: `Job::Search { seq, query }` runs
  `search::run` on a worker and answers `Event::SearchResults { seq, hits }`.
  Every request takes a new `seq`, and the UI drops an answer whose `seq` is not
  the current one, so a query typed over never shows the previous mail. The
  popup waits for the typing to stop (`SEARCH_SETTLE`, `search_settle`) — the
  title-bar box only displays the query — and Enter skips the wait;
  `search_applied` is the query the results on screen already answer, which is
  what keeps an unchanged query from searching again every frame; the empty
  query the column opens on is the applied one from the start, so the first
  frame of the process is not a change of list either. The request
  is pumped from `KimoApp::ui`, not from the message list: the popup that
  edits the query sits outside every column.

- Search is local and over everything the database holds (there is no IMAP
  `SEARCH`): `search::run` builds one statement over `messages`, joined to
  `messages_fts` when there is text. The free text becomes FTS5 clauses — a
  word is a *prefix* phrase across `{subject from_addr to_addr body_text}`, a
  CJK run is one quoted phrase per bigram in `{cjk}`, AND-ed — and every word
  is quoted, so query text can never be read as FTS5 syntax. The From/To/Subject
  boxes are `LIKE '%term%'` filters instead (with the wildcards escaped), which
  is why a sender substring like "peng" works even though the word index only
  matches word starts. Dates are `YYYY-MM-DD` read as UTC midnight (the day the
  list shows) and `before` includes that whole day. Results rank by
  `bm25(messages_fts)` when there is text and by date when there is not, and
  stop at `SEARCH_LIMIT` (the UI says "200+"). Snippets are built in Rust
  (`search::snippet`), not by FTS5's `snippet()`, because the bigram column is
  not text anybody should read.
