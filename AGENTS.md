# AGENTS.md

Working agreements for AI agents (and humans) contributing to **Kimo**.
Read this before changing anything.

## What this project is

Kimo is a desktop **email client** GUI written in pure Rust:

- **GUI:** egui (immediate mode, pinned to the 0.36 line), driven directly
  through winit + `egui-winit` + `egui-wgpu` instead of through eframe, because
  the floating title bar's blur needs the frame split in two — see the
  renderer convention below.
- **Protocols:** IMAP4rev1 and POP3 for reading, SMTP submission for
  sending, all implemented by hand in `src/protocol/` — no `imap`/`async-imap`/
  `pop`/`lettre` crates.
- **TLS:** rustls with the pure-Rust `ring` crypto provider + webpki-roots.
  `src/protocol/ffdhe.rs` adds the finite-field DH rustls does not ship (an
  FFDHE group and the two GCM `DHE-RSA` suites, the modular exponentiation
  coming from `crypto-bigint`), because some mail servers — Sina's, for one —
  offer TLS 1.2 with nothing but `DHE-RSA` and are otherwise unreachable.
  That provider is installed process-wide in `src/main.rs`.
- **HTML mail:** two renderers, switched in Preferences → Appearance and
  remembered in `settings.json` (`html_renderer`).
  *Simple* (the default) parses with html5ever (via `scraper`) and draws
  egui widgets in `src/html.rs` with a small CSS subset (`src/css.rs`):
  theme-following, selectable, clickable, no layout fidelity.  *Blitz* feeds
  the same HTML to the pure-Rust Blitz browser engine (stylo/taffy/
  vello_cpu) in `src/blitz_render.rs` and blits the rasterized page — close
  to what the sender designed, but a static image with clickable link boxes.
  Remote images and stylesheets load **by default** in both (see the
  conventions below). There is still no JavaScript.
- **Charsets:** mail-parser is built with the `full_encoding` feature
  (pulling in `encoding_rs`, pure Rust) so GB2312/GBK/GB18030/Big5/
  Shift-JIS/… mail decodes to proper UTF-8 instead of Latin-1 mojibake.
- **Sending is SMTP submission.** The compose card's Send hands its draft to
  a worker (`Job::SendMessage`), which speaks `EHLO`/`STARTTLS`/`AUTH`/`MAIL
  FROM`/`RCPT TO`/`DATA` through `src/protocol/smtp.rs`, and the message itself
  is built by `src/outgoing.rs`. A copy goes into the account's Sent mailbox
  over IMAP when the server has one. There is no drafts folder, no outbox and
  no attachments — see hard rule 4 and the compose-window convention below.
- **Tray icon:** `src/tray.rs` implements StatusNotifierItem by speaking the
  D-Bus wire protocol directly (`src/dbus.rs`) on a `std::thread` —
  deliberately not zbus (async executor, rule 2) or the GTK tray crates (C,
  rule 1). The icon is
  `assets/app.png` (256px, rasterized from the multi-color `assets/app.svg`
  — regenerate with any svg→png tool, e.g.
  `rsvg-convert -w 256 -h 256 -o assets/app.png assets/app.svg`), embedded
  with `include_bytes!` and decoded with the image crate, keeping its own
  colors (the purple circle works on any panel). Keep the source large: the
  host scales the pixmap down to its panel size, so a 48px icon looked
  blurry on a HiDPI panel. The cost is 256 KB of ARGB per `IconPixmap`
  reply, which is nothing on the session bus. Left click raises the window;
  the right-click menu is a minimal `com.canonical.dbusmenu`
  object at `/Menu` (Show / Quit) served by the same thread. Missing
  StatusNotifierWatcher disables the tray silently, and `IconPixmap` must
  stay `a(iiay)` — hosts cannot read a bare byte array. The session bus is
  silent for hours at a time, so `dbus`'s reads ride out an idle read-timeout
  expiry instead of failing on it (`read_exact` turned it into a fatal
  `WouldBlock`, and the icon vanished 30s after startup) — one rule for both
  D-Bus threads.
- **Desktop notifications:** `src/notify.rs` announces new mail through the
  freedesktop `org.freedesktop.Notifications` interface — the same raw-D-Bus
  choice as the tray, made on the shared wire layer in `src/dbus.rs` and for
  the same two reasons (no `zbus` async executor, rule 2; no C, rule 1).
  Showing one is a blocking call, so it runs on one dedicated `std::thread`
  that owns the connection: `notify::show` only posts to it, whether the
  caller is the UI or a sync worker (rule 3). The icon is the brand PNG the
  tray serves and the title bar draws, written out as a `48 · scale` px file
  under the resource cache and handed to the daemon as an `image-path` hint:
  48 logical points, which is a real 96 px bitmap on a 2× screen instead of an
  upscaled one. `image-data` is deliberately *not* used — daemons disagree
  about the byte order of its pixels — and the app icon itself is the `kimo`
  name the desktop file installs, with a `desktop-entry` hint so a shell can
  file the notification under Kimo's own entry.
  What is announced is only news: `Event::Messages` carries `new_mail`, true
  for a fetch that resumed at the folder's mark (IMAP) or found UIDLs the
  database did not hold (POP3), and never for a folder's first fill,
  `Load older mail` or the copy of a mail that was just sent. The app then
  drops what is not worth a popup: mail already read, folders whose mail is
  not *delivered* (Sent, Drafts, Trash, Junk, and Gmail's All Mail and
  Starred, where every mail of the account turns up a second time), and a mail
  the account already holds under another mailbox (the same `Message-ID`).
  Up to `NOTIFY_MAX` (3) arrivals in one batch are one notification each;
  more become a single one counting them, so catching up with forty mails is
  one popup and not forty. Preferences → General has the switch
  (`notifications` in `settings.json`, absent meaning on), and a session with
  no notification daemon is simply quiet. `cargo test notify_probe --
  --ignored` under `dbus-run-session` with `dbus-monitor` watching is how the
  wire form is checked by hand.
- **Packaging:** `packaging/linux/kimo.desktop` is the launcher entry for the
  `kimo` app_id, to be installed as
  `~/.local/share/applications/kimo.desktop`. The icon on its own
  (`~/.local/share/icons/hicolor/scalable/apps/kimo.svg`) satisfies
  `niri/window`, which looks the app_id up in the icon theme, but waybar's
  `wlr/taskbar` resolves a `Gio.DesktopAppInfo` instead and shows the grey
  `image-missing` placeholder without a desktop file. `Exec=kimo` has to
  resolve in `$PATH` — GLib silently rejects a desktop file whose `Exec`
  names a program it cannot find — which the file's header comments record.
- **Persistence:** accounts and cached mail live in one SQLite database,
  `~/.local/share/kimo/mail.db` (mode 0600, WAL), with an FTS5 index over the
  mail. `~/.config/kimo/settings.json` is the only JSON left.

## Hard rules

1. **Pure Rust only.** Never add a dependency that links C/C++ code or
   requires a system library to build: `native-tls`, `openssl-sys`,
   `aws-lc-rs`, `keyring`, etc. are all unacceptable. TLS goes through
   `rustls` (keep `default-features = false` + the `ring` feature). If a
   candidate crate needs pkg-config/Make/cmake at build time, it is wrong
   for this project. (A WebKitGTK "web view" renderer used to exist behind
   an optional `webview` feature; it has been removed. Do not reintroduce
   C-library exceptions to this rule.)
   **One exception, approved by the project owner:** `rusqlite` with the
   `bundled` feature, for the mail database. `bundled` compiles the SQLite
   amalgamation (with FTS5) through `cc` at build time and links it
   statically — no system library, no pkg-config, no runtime dependency —
   because the storage the project asked for is specifically SQLite + FTS5.
   It is the only C in the tree: do not let it become a precedent, and do not
   swap it for `bundled-sqlcipher`, a `libsqlite3-sys` built against a system
   library, or any other C crate.
2. **No async runtime.** Networking runs on short-lived `std::thread` jobs
   (see `src/sync.rs`); results come back over `std::sync::mpsc`. Do not
   introduce tokio, async-std, or an executor.
3. **Never block the UI thread.** `KimoApp::ui` only drains the
   event channel and draws. Any socket I/O, file scan or other potentially
   slow work must be a `sync::Job` on a worker thread. Workers call
   `ctx.request_repaint()` when they emit.
4. **Sending is SMTP submission and nothing else.** The compose card's Send is
   wired (`Job::SendMessage` → `protocol::smtp` → `outgoing::render`), and the
   one thing it writes besides the mail itself is the copy in the account's
   Sent mailbox (`APPEND`, IMAP accounts only). Deliberately still out of
   scope, each its own decision by the project owner: drafts of any kind
   (nothing about a draft is persisted), an outbox/queue or any retry, file
   attachments, HTML bodies, DKIM or any signing, and OAuth2/XOAUTH2 — SMTP
   authentication is `AUTH PLAIN`/`AUTH LOGIN` over TLS.
5. **Keep the dependency tree small.** Every new crate needs a concrete
   justification; prefer std where reasonable.
6. **UI strings are English.** CJK rendering is handled by the best-effort
   font fallback in `fonts.rs::install` — do not add an i18n
   framework.

## Commands

| Task          | Command                                          |
| ------------- | ------------------------------------------------ |
| Build         | `cargo build`                                    |
| Run           | `cargo run`                                      |
| Tests         | `cargo test`                                     |
| Lint (strict) | `cargo clippy --all-targets -- -D warnings`      |
| Format        | `cargo fmt`                                      |

Everything must be green (build, tests, clippy, fmt) before you call work
done. There is no CI yet — that discipline is on you.

## Architecture

```
src/main.rs             entry point; the winit event loop, the window, egui's input state
src/gpu.rs              the wgpu side: device, surface, egui's renderer, and one frame
src/app.rs              all UI: KimoApp state, panels, preferences modal, event handling
src/models.rs           Account / Protocol / TlsMode / MailMessage (plain types: nothing is serialised any more)
src/error.rs            MailError + MailResult
src/fonts.rs            installed-font discovery (ttf-parser scan) + egui font installation
src/storage.rs          XDG paths, settings.json, remote-resource cache, Mailbox/Folder (in memory)
src/db.rs               the mail database: schema + migrations, accounts, folders, messages, the old-JSON import
src/search.rs           mail search: the CJK bigram index, the FTS5 query builder and the filters they run against
src/threads.rs          mail grouped into conversations: the message list's own model of a folder
src/theme.rs            theme choice + the light/dark color schemes and their egui Visuals
src/mail.rs             raw RFC 5322 bytes → MailMessage (mail-parser; HTML kept, attachments
                        as name/type/size, one part back out for a download)
src/outgoing.rs         the mail being sent: addresses, headers, encoding → RFC 5322 bytes
src/html.rs             HTML mail → egui widgets (subset: blocks, lists, tables, links, inline styles)
src/blitz_render.rs     HTML mail → rasterized page via Blitz (headless render, net provider, fonts)
src/css.rs              tiny CSS subset (decls, tag/class/id selectors) for HTML mail
src/folders.rs          folder kind (name → special mailbox, incl. CJK) + embedded row icons
src/glass.rs            the floating title bar's frosted backdrop (wgpu passes + a paint callback)
src/render_bench.rs     test-only: times both HTML renderers for the same bodies (bench_render)
src/dbus.rs             the session bus: D-Bus wire protocol (marshalling, framing, the connection)
src/tray.rs             StatusNotifierItem over the session bus (std::thread) + svg→ARGB icon
src/notify.rs           new-mail notifications over the session bus (std::thread) + their 48·scale icon
src/sync.rs             Job/Event enums, worker threads, per-protocol sync logic
src/protocol/mod.rs     protocol modules
src/protocol/stream.rs  TcpStream/TLS transport (MailStream, STARTTLS upgrade, buffered Conn)
src/protocol/ffdhe.rs   FFDHE kx group + DHE-RSA suites, which rustls does not ship
src/protocol/imap.rs    minimal IMAP4rev1 client (LOGIN, LIST, SELECT, FETCH w/ literals,
                        UID STORE, APPEND, delete: UID FETCH probe + UID EXPUNGE)
src/protocol/pop3.rs    minimal POP3 client (USER/PASS, STAT, UIDL, RETR w/ de-stuffing, DELE, QUIT)
src/protocol/smtp.rs    minimal SMTP submission client (EHLO capabilities, STARTTLS,
                        AUTH PLAIN/LOGIN, MAIL/RCPT/DATA w/ dot-stuffing)
```

### Data flow

```
UI (app.rs ui)
   │  spawn(Job)                                   UI thread — no sockets, no DB writes
   ▼
worker thread (sync.rs)
   │  ImapClient / Pop3Client / SmtpClient         blocking I/O is fine here
   │  db::save_messages / db::set_seen             the only place the DB is written
   ▼
Event (mpsc) ──► drain_events() at frame start ──► merge into AccountUiState
```

- The UI thread touches the database exactly once, in `KimoApp::new`
  (`db::load_accounts` + `db::load_mailboxes`, replacing the JSON parse that
  used to happen there); every frame after that works from the in-memory
  `AccountUiState`, and every write is a worker's (see `src/db.rs`).
- Merge identity: IMAP messages are keyed by `(UIDVALIDITY, UID)` per folder
  (a UIDVALIDITY change clears the folder); POP3 messages are keyed by their
  UIDL string, and `\Seen` is local-only. The database does the same in SQL:
  `db::save_messages` upserts on `(account_id, folder, uid)`, deletes a
  folder's rows first when its UIDVALIDITY moved, and keeps `seen` monotonic
  (`max(messages.seen, excluded.seen)`).
- Remote HTML-mail resources (images, stylesheets) load by default — an
  explicit decision by the project owner, accepting that this leaks the
  reader's IP and open timing to senders (tracking pixels). Fetches run as
  `Job::FetchImage`/`FetchStylesheet` on worker threads via ureq (pure
  Rust, rustls + ring), are capped in size, cached on disk at
  `~/.local/share/kimo/cache/resources/` (SHA-1 of the URL) and held in
  `KimoApp::images`/`stylesheets`. The renderer reports missing URLs via
  `html::HtmlContext`; never fetch on the UI thread.
- Syncing is incremental, and the state that makes it so lives in the database:
  a folder carries its marks (`folders.uid_next` and `folders.first_num`, read
  through `db::FolderSync`). IMAP resumes at `uid_next` — `UID FETCH <next>:*`,
  the `by_uid` half of `imap::fetch_bodies`; a mark from another UIDVALIDITY
  generation is dropped and the folder fetched afresh — while POP3 asks for the
  UIDLs it does not already store (`db::stored_uids`). `db::save_messages` moves
  an IMAP folder's mark to one past the highest UID it holds, in the same
  transaction as the batch, so a crash can never advance past mail that did not
  reach the disk. `BACKFILL_LIMIT` (in `sync.rs`) is the size of a folder's
  *first* fetch: the newest messages only, and the rest waits for the button in
  the bullet below. `Job::Sync` (startup
  and the title-bar button) fetches the folder list, INBOX and every folder that
  has been fetched before, and a folder that fails does not cost the others
  theirs (the first error is reported at the end). A folder the user never
  opened has no mark and waits for its first click, which is where its backfill
  happens; a warm folder with nothing new neither writes nor emits. Each
  batch it emits carries `new_mail` — true only for mail the fetch found
  beyond the folder's mark, so a backfill, a `Load older mail` and the copy
  of a mail just sent are all false — which is what a desktop notification is
  raised from (see the notifications bullet above). The working
  mode (Preferences → General; `settings.json`'s `working_mode`, absent meaning
  the build) can switch the automatic half of that off: in **Debug** neither the
  startup sweep nor the fetch-on-first-open happens, the cached mail still loads
  from the database, and only the title-bar Sync button — which only Debug
  shows, since Release has nothing for it to add — and `Save & sync` in
  Preferences reach the server. The button sweeps every account and, for the one
  whose folder is open, fetches that folder even when it has no mark: the column
  says "Not fetched yet — click Sync in the title bar" for exactly that folder,
  and a folder with no mark is not part of a sweep — `sync_queue` is what puts
  it first, so the message is not a lie in the one mode that shows the button. It is a per-installation setting rather than a
  build flag so that a development build can be pointed at a real account
  without a sync on every start.
- Older mail is fetched on demand: the message list floats a `Load older mail`
  button over its foot when the list is at its end and the folder's marks say
  the server has mail below everything here, and one click is a `Job::LoadOlder`
  of `OLDER_BATCH` (50) messages. Two marks answer that without asking the
  server on every scroll: `folders.first_num`, the floor a folder has been
  fetched *down to* (IMAP: the lowest UID a fetch was sent for; POP3: the lowest
  message number), and `folders.oldest_uid`, how far down the server goes — what
  `FETCH 1 (UID)` answered, which every IMAP sync records, and 1 for POP3, whose
  numbering starts there. `db::older_available` is the whole rule,
  `db::load_mailboxes` derives it at startup and `Event::OlderMail` carries a
  fresher answer. A click re-asks before fetching, so an emptied mailbox clears
  the button rather than failing on it; the floor moves even when the batch came
  back empty (the mailbox has gaps), which is why it is a mark about what was
  *asked for* — deletes leave it alone, and a UIDVALIDITY rotation clears it.
- The folder list is cached like the mail is, and the `LIST` is only what
  updates it. `folders.listed` records which mailboxes the last answer named;
  `db::save_folder_list` writes it in one transaction with the folders
  themselves — the ones the answer left out are marked unlisted, never deleted,
  because the mail cached for them is still the only copy of it here — and
  `db::load_mailboxes` carries the flag into `AccountUiState::folders`, which is
  what the navigation column opens on. So a start shows the folders the last
  `LIST` named instead of standing on an offered INBOX until a sync runs, and a
  sync that finds the same mailboxes changes nothing on screen. Two things are
  deliberately *not* a mailbox list: an answer that names no mailbox at all
  (`imap::list_folders` reports it as the protocol error it is, rather than
  saying the account has no folders), and one whose `LIST` **failed** — that
  leaves the stored list and its flags alone and reports the error on the
  account, while the sweep falls back to `db::listed_folders` — the mailboxes
  the last answer named — so a server that will not list its mailboxes can still
  have their mail fetched. That list is a fallback, not a display source: the
  column draws what `LIST` last said. Mail fetched for a folder proves the
  server has it, so `save_messages` lists it again — the one way back.
- The Blitz renderer is a `Job::RenderHtml` on a worker thread: it parses,
  styles, lays out and rasterizes the body, resolving every sub-resource
  through `blitz_render::MailNet` (a blocking `NetProvider` that reuses the
  on-disk resource cache, and understands `data:` and `cid:` URLs). The page
  comes back as `Event::HtmlRendered` with premultiplied RGBA tiles and link
  boxes, which `app.rs` uploads as textures. Pages are keyed by mail +
  quantized *width* — never the pane height, which only feeds Blitz's viewport
  (`vh` and friends): keying on it re-rasterized an identical page on every
  window resize. At most two pages are cached; a cached page is reused, scaled
  down and never up, while the pane stays within `BLITZ_REUSE_RATIO` of the
  width it was laid out for; and a render starts only once the geometry has
  held still for `GEOMETRY_SETTLE`, so a splitter drag or an animated window
  resize queues one render at the end instead of one per frame (the built-in
  renderer waits the same time, see the next bullet). Every
  request takes a fresh `blitz_render::Cancellation` ticket: a render whose
  ticket is stale gives up mid-layout (and between raster slices) and reports
  *nothing*, so a superseded render can never be mistaken for a failure of the
  newer one. Never render Blitz HTML on the UI thread.
- The built-in renderer keeps two things between frames in `KimoApp::body`
  (`BodyCache`), because a splitter drag moves the reading pane's width on every
  frame and the whole body is otherwise re-derived on each one: the *parsed
  body* (`html::Document`, since html5ever's parse is the larger half of what a
  frame costs and the tree does not change while the mail is open) and the
  *width it settled at*. There is one entry per mail on screen — a conversation
  shows several, and the pane asks for every one of them on every frame — up to
  `BODY_CACHE` of them, the mail off screen longest being the one a new body
  evicts. The body is laid out at that width while the pane is
  moving — in a scope of its own, because a ui never gets narrower than what it
  has already drawn (`set_max_width` widens the max back to the min rect) — so a
  drag clips the mail instead of re-shaping every run of text in it; once the
  geometry has held still for `GEOMETRY_SETTLE` it reflows once, for the width
  the pane came to rest at. A new mail, or a body re-fetched under the same uid
  (`body_fingerprint`), drops the document and is laid out at the pane's own
  width at once. Do not draw the body outside that scope, and do not "fix" the
  hold by narrowing the card's own ui.
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
- Sending is a job like any other, with two differences. `Job::SendMessage`
  renders the draft (`outgoing::render`), connects, authenticates when the
  server advertises a mechanism this client speaks, and hands the message to
  `DATA`; the outcome comes back as `Event::MessageSent { recipients, copy }` or
  `Event::MessageSendFailed { error }`. The first difference: it is *not*
  bracketed with `SyncStarted`/`SyncDone` — the compose card that asked is on
  screen, cannot be closed while the mail is in flight, and says `Sending…` —
  and a failure is the card's error, not the account row's warning. The second:
  `Event::MessageSent` is what closes the card, because until the server says it
  has the mail the draft is the only copy of it. `SentCopy` is what became of
  the copy in Sent: `None` for a POP3 account or a server that lists no Sent
  mailbox (`folders::kind_of` is the table that recognizes `Sent`, `Sent
  Items` or 已发送邮件, whatever wire form the name arrived in), `Saved {
  mailbox }` after an `APPEND` and the incremental fetch of that folder on the
  same connection — so what was sent is in Kimo's Sent when the user opens it,
  not at the next sync — or `Failed`, which un-sends nothing and is what the
  notice says.
- Remote mutations are `UID STORE +FLAGS.SILENT (\Seen)` (IMAP) and, behind
  the title bar's Delete button, `\Deleted` + `UID EXPUNGE` (IMAP) or `DELE`
  (POP3). A delete tells the server first and deletes locally after; a message
  the server no longer has is deleted locally only, and a select-mode selection
  is one connection carrying every mail of one folder. See the deletion bullet
  under Conventions for the whole rule.

### Storage layout

- `~/.local/share/kimo/mail.db` — the one database (SQLite, WAL, mode 0600
  because it holds the passwords). `accounts` (id, name, protocol, host,
  port, username, password, tls, smtp_host, smtp_port, smtp_tls, smtp_username,
  smtp_password, position), `folders` (account_id, name,
  uidvalidity, uid_next, first_num, oldest_uid, synced, listed — the fetch
  marks of the bullets above, then `listed`, what the last `LIST` named),
  `messages` keyed by `(account_id, folder, uid)` with
  the subject, addresses, both bodies, the preview, the bigram column and the
  three headers a conversation is stitched from (`message_id`, `in_reply_to`,
  `refs` — the last one is not `references`: that is a SQL keyword),
  `attachments` (the name, the MIME type and the decoded size — never the
  bytes; see the attachment-card convention) and `inline_images` (raw bytes,
  not base64) as children of a message, and the external-content FTS5 table
  `messages_fts`, which only the
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
  version-6 step is that kind of step too: it adds an attachment's `content_type`
  and `size`, and derives nothing, because the part a mail arrived in is not
  stored — the rows it holds keep an empty type and a zero size, and only mail
  fetched after the step carries them. The version-7 step is the third of that
  kind: it adds the threading headers, which were never stored either, and a
  mail from before it threads by subject instead (`crate::threads`).
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
  `Job::Sync { purge }`, so nothing can reorder those two steps). An account's
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
  `egui`), `working_mode` (`debug`/`release`, absent means the build — see the
  syncing bullet above), `threaded` (whether the message list groups mail into
  conversations; absent means the app's own default, which is on), `auto_sync`
  and `auto_sync_minutes` (the automatic sync's switch and its interval, both
  absent by default — see the automatic-sync bullet below), `notifications`
  (whether new mail is announced on the desktop; absent means on, see the
  notifications bullet above), and the font
  families picked in Preferences →
  Appearance (`ui_font`, `mono_font`; absent means egui's built-in font plus
  the CJK fallback). Family *names* are stored, not paths, so the file survives font
  updates; a name that no longer resolves is simply ignored. Older files may
  still carry a `render_mode` field from the removed WebKitGTK renderer; it
  is ignored on load — do not reuse that name. The palette that the theme
  resolves to is *not* in this file; it is a compiled-in default, see the
  color-scheme bullet under Conventions.
- `~/.local/share/kimo/cache/notify/icon-<px>.png` — the brand icon a
  notification hands the daemon through its `image-path` hint, one file per
  pixel size (`48 · screen scale`, see the notifications bullet above). It
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

## Conventions

- Errors: return `MailResult<T>`; convert with `?` (From impls exist for
  io/rustls/serde_json/rusqlite). `MailError::Db` is SQLite's own error and
  `MailError::Storage` everything else the storage layer can fail at. Map
  server auth refusals to `MailError::Auth` so the UI shows them distinctly
  from protocol bugs. `MailError::Refused` is a server that read a command and
  said no (IMAP NO / BAD), kept apart from `Protocol` — a broken connection or
  a reply we cannot parse — so a command with a fallback (the mailbox-wide
  `EXPUNGE` for a server without UIDPLUS) can tell the two apart.
- Protocol clients are synchronous and single-purpose; add a method only
  when a sync job actually needs it. The three of them map a server's own
  refusal the way the first two always have: a 4xx/5xx (SMTP) or a `-ERR`
  (POP3) the server was asked for is `MailError::Refused`, one for a command it
  never got is `Protocol`, and credentials the server said no to are
  `MailError::Auth` — `smtp::login` rewrites its refusals into that, the way
  `imap::login` does.
- Finite-field DH lives in `src/protocol/ffdhe.rs` and is deliberately narrow:
  2048-bit groups only, and the `DHE-RSA` suites are appended *behind*
  *ring*'s, so any server offering TLS 1.3 or ECDHE still gets that — DHE is a
  fallback for Sina-class servers, never a preference. rustls matches the
  parameters in a server's `ServerKeyExchange` against a provider's
  `ffdhe_group()` by exact equality, so a group whose prime is not RFC 7919's
  has to be pinned: Sina's fixed 2048-bit safe prime (verified stable across
  connections and hosts) is registered under its own code point,
  `NamedGroup::Unknown(0x01ff)` — `Unknown` is rustls' documented answer for a
  group the `NamedGroup` enum has no name for, and servers ignore codes they
  do not know. Supporting another such server means adding its prime to
  `GROUPS`, not loosening that equality check.
- IMAP mailbox names stay in their wire form (possibly modified UTF-7) in
  `Event`s, state and the database; `imap::decode_mailbox_name` decodes them for
  display only. SELECT/UID STORE must always use the wire form — do not
  re-encode decoded names by hand.
- Pure parsing helpers (e.g. `extract_uid`, `literal_size`, `html_to_text`)
  live next to their users and get `#[cfg(test)]` unit tests — that is where
  tests go in this codebase; no `tests/` directory yet.
- egui is pinned to the `0.36` line, with `egui-winit`, `egui-wgpu` and `wgpu`
  moving in lockstep, and the step from 0.31 was not a version bump but a change
  of shape: 0.36 draws the whole app into one root `Ui` (`KimoApp::ui` — there
  is no `Context`-level `update` any more), the side panels are `egui::Panel::left`/`right` shown *onto* that `Ui`
  (with `min_size`/`default_size`, not `min_width`/`default_width`), the
  context's `screen_rect`/`available_rect` are `viewport_rect`/`content_rect`
  (the message list's own rect is taken from `mail_list`, which records it),
  `Ui::allocate_new_ui` is `Ui::scope_builder`, a galley needs
  `Context::fonts_mut`, and the styles are *per theme*: `theme::install` fills
  in both of them and sets the preference from Kimo's own system detection, so
  a stock widget is never drawn in egui's palette. The layout tests run their
  frame through `run_frame`, because epaint panics on a dropped
  `TexturesDelta`. The integration is `src/gpu.rs` + `src/main.rs`, not
  eframe: eframe owns the frame's render pass, and the glass needs it split in
  two (see the frosted-backdrop bullet). `main.rs` is a winit
  `ApplicationHandler` — window, `egui_winit::State` for input, egui's repaint
  callback turned into a `ControlFlow::WaitUntil` timer (a deadline that has
  not come round yet must be *left in place*: a loop that consumes it sleeps
  through the frame egui asked for), and
  `egui_winit::process_viewport_commands` for the commands the title bar sends
  (drag, minimize, maximize), with two catches that are the loop's own to
  handle: a close is only *recorded* by it, as a `ViewportEvent::Close` in the
  `ViewportInfo` it is handed (in eframe that recording is the app's chance to
  refuse the close, via `CancelClose`), so the loop reads it and ends itself —
  the tray's Quit takes the same path — and that info has to be refreshed from
  the window every frame (`egui_winit::update_viewport_info`) and handed to
  egui as `RawInput::viewports`, because `egui-winit` itself fills in nothing
  but the scale factor: without it `i.viewport().maximized`, which draws the
  title bar's maximize/restore glyph, is always `None` and the maximize button
  can never restore. The renderer is wgpu, and its 0.36-era API
  is young: `InstanceDescriptor` is built by `new_with_display_handle`,
  presenting is `Queue::present`, pipeline layouts take
  `bind_group_layouts: &[Some(&layout)]` and `immediate_size`, passes and
  pipelines take `multiview_mask`, and a `TexturesDelta` must be emptied
  (`set.clear()` as it is uploaded, `free` when the frame is on its way)
  because epaint panics on one that is dropped unapplied. Keep `egui`,
  `egui-winit`, `egui-wgpu` and `wgpu` in lockstep when bumping, and read the
  egui CHANGELOG before doing it.
- Layout is a three-column mail client: left navigation (collapsible via
  the title-bar toggle; auto-collapses when the window is narrower than
  640px — see `NAV_AUTO_COLLAPSE_WIDTH`), the central column (while a search is
  active the list shows `search_results` instead of the selected folder, and
  `message_card` paints both kinds of row), and the reading pane on the right
  with a "No mail selected" placeholder. Collapsing the navigation — by the
  toggle or by the narrow-window rule — slides the column out rather than
  dropping it: `Panel::show_collapsible` keeps the rows at the layout they had
  at the column's full width and slides the whole column off toward the
  window's edge, clipped, so the list takes its room over the frames of egui's
  animation time (`Style::animation_time`, 0.2 s), and the width the splitter
  gave the column survives the slide — a collapsed column reopens as wide as it
  was. The splitter is not a second way to collapse (`drag_to_open(false)`,
  and the expanded flag `show_collapsible` writes back is dropped): the toggle
  stays the one control, the splitter keeps clamping at `min_size` the way it
  always has, and a collapsed column leaves no invisible grab strip — or resize
  cursor — over the message list's left edge.
  Folders indent under their account (`Ui::indent`) without egui's faint guide
  line down the left of the block (`indent_has_left_vline` cleared in
  `nav`): in this column that line reads as a rule through the folder list.
  The chrome — title bar and navigation — is the scheme's `panel`; the message
  list and the reading pane are the lighter `content`. The columns run from the
  window's own top edge, and the title bar floats over them (see the title-bar
  bullet below): the navigation is inset by the bar's height and never goes
  under it, while the two content columns keep clear of the bar with padding
  *inside* their scroll areas, which scrolls away with what they hold — the
  list's rows, and the reading pane's cards, their headers included, which
  is why mail passes under the glass there too. **The list column is a card
  too** (`list_card`): the panel behind it is chrome, and the card is as tall as
  the list's own content — the bar's height and the rows, and *nothing* below
  the last of them — so a folder with three
  mails leaves the chrome below them instead of a column of empty surface, a
  list that outgrows the window fills the column as it always did, and a column
  with nothing to list (no account, an empty folder, a search with no hits) has
  no card at all, the way the reading pane has none with no mail open. Its height
  is known only once the list has been laid out, so `mail_list` reserves the
  shape and fills it in at the foot of the frame — under the rows, which is
  where it has to be drawn. The card is framed by a hairline down each side,
  drawn for the card's own height rather than the column's, and the reading pane
  is a card of `content`
  (`CONTENT_CARD_CORNER_RADIUS`) inset by `CONTENT_GUTTER` into the chrome,
  whose left side doubles as the splitter; with nothing selected there is no
  card at all. The first card of what the pane shows starts *below* the floating
  bar — the bar's height is space above it in the scroll area, not padding
  inside it — so its top edge and its two rounded corners are on screen; the
  space scrolls away with the card, which is what still lets the mail pass under
  the glass. **A conversation is that card once per mail** — not a card
  holding a card per mail — stacked oldest first with `CONVERSATION_CARD_GAP`
  of chrome showing between two of them, so every mail of a thread sits on the
  pane the way a mail on its own does. Each card is drawn by the pane and not
  by a frame around its mail: `mail_card` reserves the shape and the pane
  fills it in from the pane's own left edge to its right one, because a card is
  the pane's surface — a mail wider than the pane scrolls sideways over its
  card instead of dragging the card off the pane with it. The cards are the
  pane's scroll container — headers, rule,
  body and the attachment cards at the foot of each mail scroll together, and
  the scroll bar runs down the pane's edge rather than down a card's — and a
  card still hugs its mail: it is exactly as tall as the mail until the mail
  outgrows the pane. The scroll area scrolls *both* ways, which is
  what keeps the splitter honest: with horizontal scrolling off, a mail whose
  layout cannot be narrower than the pane (a fixed-width table — most HTML
  mail) widened this scroll area, and through it the panel's *content* rect,
  which is the rect egui persists as the panel's width for the next frame
  (`PanelState`); a splitter drag therefore sprang back to the mail's width on
  release, and since egui's `Grid` keeps its column widths between frames the
  pane could never be dragged narrower than the widest it had ever been for
  that mail. The horizontal bar appears only when the mail really is wider than
  the pane, and a newly opened mail resets both offsets. The list lays out only
  the rows its viewport shows (`virtual_rows`, which is
  `ScrollArea::show_rows` with this column's arithmetic: the first row starts
  below the title bar's padding, inside the scroll area, so a row range counted
  from the content's own origin would be off by it — and the row a partly
  scrolled viewport starts in could be skipped), and it draws them straight out
  of the account state: mail is never copied to be listed, so a click is
  remembered as the mail it landed on and opened once the list has stopped
  borrowing that state. Both highlights in the list are the mail's whole row: a square
  block spanning the column edge to edge. A mail draws its own edges instead of
  sharing a rule with its neighbour — left, right and bottom in `row_edge`, the
  top in the fainter `row_edge_top`, each a pixel inside its own rect — so what
  separates two mails is this row's top edge against the bottom edge of the row
  above: two pixels, and `item_spacing.y = 0` keeps the layout from adding a
  third. The list's first row draws no top edge — there is no mail above it to
  be separated from, and the edge would land on the title bar's bottom edge,
  which is deliberately bare. Which row is first comes from the caller
  (`message_card`'s `first` flag) rather than from the row's y: the bar's own
  height is padding above the first mail, so it no longer starts at the
  column's top edge. The column's hairlines are `row_edge` too and run its whole height,
  and a row paints them again over its own highlight, which is what keeps a
  selected mail's frame visible across its fill. Pane padding lives *inside*
  each scroll area
  (`Frame::inner_margin(LIST_PADDING)` around the rows — the sides, and the
  bar's height at the top, which is what the rows scroll under; there is no
  bottom padding, because the list's card ends with its last mail — and
  `mail_card` for each mail: the card's own padding all round, and — above the
  first card of what the pane shows, and only that one — the bar's height as
  space in the scroll area rather than padding on the card), not on the panel's
  frame, which is what lets the scroll bars reach
  the column's edge and the pane's edge instead of stopping on the padding.
  egui's panel separators are off in both schemes (they would double those
  hairlines and the card's gutter) and the two boundary pixels are repainted
  with the chrome after the panels, so hovering a splitter draws no line at all.
- Search lives in the title bar and in the popup it opens, never in the
  column. The title-bar box shows the query with the magnifier drawn inside its
  left edge, and the whole box is one click target: a click anywhere in it —
  glyph or text — opens the popup, as does Ctrl+F. The field in it is
  deliberately *not* interactive (it displays the query; the popup is the
  editor): a caret there would fight the popup's own field for the keyboard and
  leave focus behind when the popup closes, and one string backs both, so
  whatever is typed in the popup is what the box shows. `search_modal`, an
  `egui::Modal`, holds the free text, the scope combo and the filter grid
  the column used to carry, with the live result count along its bottom; Enter
  in any of its boxes (or the Search button) applies the query and closes it,
  and Clear empties it without closing, so closing the popup is never a
  cancel. Nothing else about a search is drawn: the message list starts
  straight at the rows — the folder's mail or the hits — because a header over
  them was noise the project owner asked to drop, and the magnifier is the only
  sign left. (Select mode's select-all row is not that header: it is there only
  while the mode is.) It is grey while the box is empty —
  `visuals.weak_text_color()`, the color of the hint text it sits next to, so
  the box reads as an empty placeholder rather than a live control — and
  accent-colored once a query is what the column shows.
  Nothing about a query is persisted.
- A message row (`message_card`; the folder list and the search results share
  it, only the third line differs — the preview, or *account · folder — match*)
  separates unread mail by *weight*, not by color: the sender
  and the subject always take the scheme's `text` (read mail is not dimmed),
  the preview always takes `text_weak`, and an unread row's sender and
  subject are struck twice half a point apart (`paint_row_text`, see
  `BOLD_STRIKE`). That strike is a deliberate stand-in for real bold: epaint
  has no synthetic bold (`FontTweak` only scales and shifts glyphs) and the
  picked face may ship no bold at all — Adwaita Sans, the desktop default
  here, has only Regular and Italic, which is why the fonts module's
  one-face-per-family model cannot supply one. The second strike is sub-pixel
  and does not change the layout the galley was measured for, so the ellipsis
  cut and the right-aligned timestamp stay exactly where they were. The row's
  font sizes are the `ROW_DATE_FONT_SIZE` / `ROW_TITLE_FONT_SIZE` /
  `ROW_PREVIEW_FONT_SIZE` constants next to `MESSAGE_ROW_HEIGHT`, and where
  they sit are `ROW_SENDER_Y` / `ROW_SUBJECT_Y` / `ROW_PREVIEW_Y` (a 68 pt
  row, its three lines placed as one block centred in it: the room above the
  sender's line is the room below the preview, which
  `the_rows_three_lines_are_centred_in_the_row` pins), so retuning the list is
  one edit instead of a hunt for literals. The three lines are elided by
  epaint itself (`truncate_galley`: one `LayoutJob` whose `max_rows` is 1 and
  whose overflow character is the ellipsis), never by a search that lays a
  galley out per probe to measure it: the list re-lays out every visible row on
  every frame the column's width moves — a splitter drag, the navigation
  column's slide — and one shaping pass per line per frame is the whole of what
  that costs (`bench_nav_slide` measures it). A search's probes are largely
  cache-warm from the width of the frame before, but the frames where they are
  not pay `log2(chars)` shapings apiece — its worst frames cost about three
  times what the elision's do. A mail with parts gives the end of its title line
  to a paperclip (`paint_clip_icon`, one stroked path in the title bar's own
  24×24 outline style, drawn in the row's `text_weak` however unread it is),
  and the title is elided `ROW_CLIP_WIDTH` + `ROW_CLIP_GAP` earlier for it;
  `ROW_CLIP_SIZE` is the box the glyph is drawn in, which is taller than it is
  wide because a clip is. Both lists answer the same question — a folder row
  from `MailMessage.attachments`, a search hit from the count the query
  returned — so a hit no longer spells the paperclip out in its third line: the
  glyph is the one way the list says it.
  **A row offers its own delete button under the pointer**: a `ROW_ACTION_SIZE`
  (24 pt) cell at the right end of its first line, where the timestamp sits —
  the timestamp, and a conversation's count before it, gives way to a muted
  trash glyph (`paint_delete_icon` at `ROW_ACTION_GLYPH`), which takes the
  scheme's `danger_hover` fill with the `on_danger` glyph once the pointer is on
  the button itself — the treatment the title bar's close button gets, for the
  same reason: the button is the confirmation, and what it does cannot be
  undone. What is *laid out* on that line does not change with the hover — the
  sender is elided against the timestamp's slot either way — so nothing on the
  row moves under the pointer; only what is painted does. The button is a
  target on every row, drawn or not, like the box: a target that only appears
  once the pointer is already on it is one a click can miss. **What it takes is
  the mail the row speaks for** — a mail row's own mail, a conversation's
  *newest* one, which is the mail the row opens in the pane and the one the
  title bar's Delete would
  take — and not the whole thread: that is the row's box (all of it at once) or
  the pane's mail through the title bar (one at a time). A conversation's hover
  text says the newest mail outright; the title bar's own wording is used for a
  mail. A search hit's button is keyed by the hit, so it deletes where the hit
  lives and never opens the folder instead. It is offered in **both modes** — a
  row is a row — so select mode's box checks and its button still takes that one
  mail, without leaving the mode or touching the selection.
- The message list is a list of **conversations** by default: `crate::threads::group`
  turns a folder's mail into threads — a reply joins its parent through
  `References` read from the end (the last id this folder holds is the parent),
  or through `In-Reply-To` when the client wrote no chain, and a mail whose
  headers say nothing joins the thread of the subject it answers (a prefix like
  `Re:` or `答复:` stripped, either colon; two mails that merely share a subject
  stay apart) — and the list draws one row per conversation. A conversation's row
  is its own: the newest mail's sender, date and preview under the thread's
  subject, the count of what it holds beside the date, a paperclip when any of
  its mails has one, and a chevron at the left: a thin line drawing kept at a
  subdued strength of the row's own weak color (`THREAD_CHEVRON_ALPHA`), coming
  up to the full color under the pointer — a control at the edge of a row, not
  part of the mail. A conversation of one is not a conversation: it is a plain
  mail row, with nothing in the chevron's column, and the pane shows it the way
  it always has. **Every row is inset by that column**, conversation or not
  (`RowPlace::indent`): the avatars line up down the whole list, and a mail with
  nothing to open leaves the column as the placeholder it is rather than starting
  a step further left than its neighbours. Select mode puts a checkbox in each
  of those slots instead of the circle — see the select-mode bullet below. The
  one exception is a mail of an *open* conversation: it steps in `THREAD_STEP`
  further and draws a smaller avatar (`THREAD_AVATAR_DIAMETER`,
  `RowPlace::avatar`), which — with the chevron's column left empty above it — is
  what says at a glance that the mail belongs to the conversation over it. **There is no guide line**: no line is
  drawn down that column for a thread's mails, and the step and the smaller
  circle are the whole of the mark.
  **The row and the chevron do different things.** The *chevron* opens and closes
  the thread's mails under it in the list — and does nothing else, so the list's
  shape only ever changes when the user asks for it. The *row* opens the
  conversation in the reading pane: every mail it holds, oldest first, each on
  the pane's own card (`mail_card`) — the cards stacked with
  `CONVERSATION_CARD_GAP` of chrome between them and nothing drawn around
  them — so a thread reads as the mails it is made of rather than as one long
  document with three sets of headers in it. A mail on its own is that same
  card, once: same edges, same width, which is what the pane's card test pins
  (`a_conversation_draws_one_card_per_mail_on_the_pane`). That is
  `KimoApp::open_conversation`, which takes the thread the selection is in
  from the list's own model; with nothing threading, or a model that does not
  cover the folder, the pane shows the mail alone. The pane's scroll offset is
  keyed by the *conversation* (`Conversation::key`), so opening another mail of
  the thread
  already on screen does not jump back to its top.
  The chevron is a click target of its own (`ui.interact` over the row's, with
  `RowResponse` reporting the two), it is a line drawing rather than the filled
  triangle the navigation column's sections use, and it is the only thing that
  writes `open_threads`, which is in memory only and keyed by `Thread::key` (the
  thread's oldest uid, stable while the thread is). Because the rows a frame drew
  are the ones it had before the click, a toggle asks for a repaint itself: a
  pointer that stops moving asks for no frame of its own, and the list would sit
  there looking as if the click was lost.
  An open conversation's mails are rows of their own under it, oldest first,
  each a step in with a smaller avatar and nothing drawn down that left column —
  `RowPlace` is what a row draws there, and only a conversation's own row draws
  anything in it: its chevron. Every row is
  `MESSAGE_ROW_HEIGHT` tall whatever it is, which is what keeps `virtual_rows`
  honest: threading changes *which* rows there are, not how tall one is. The rows
  are derived rather than built in the drawing: `KimoApp::list_rows` (`ListRows`)
  holds the threads and the rows until the folder's mail
  (`AccountUiState::folders_rev`, which a mail marked read moves too), the
  account or folder, the threading switch, or the set of open threads
  (`open_rev`) changes — grouping a folder is a map, a union-find and a sort over
  all of it, and the list is drawn on every frame.
  **Search results are deliberately not threaded**: they are a flat list of hits,
  and where a hit sits among its own folder's conversations is not what was
  asked for. Threading is a preference (`threaded` in settings.json,
  Preferences → General) and per folder, like the rest of the list.
- The message list has **two modes**, and which one it is in is what a click on
  a row means. **View mode** is the list as it always was: the row opens the mail
  — or the conversation — in the reading pane, and the avatar stands at its left.
  **Select mode** is the list as a checklist: every row draws a checkbox in the
  avatar's slot instead, the box takes checks, and a click anywhere else on the
  row leaves the mode instead of opening a mail — the way out sits beside the way
  in, because the mode is a temporary errand rather than a place to be.
  The way *in* is the pointer: in view mode the row under it trades its avatar
  for an empty box — `CHECKBOX_SIZE` (18 pt) in the 32 pt slot the circle fills,
  since a box that size reads as a control while the circle's size would read as
  a button — and a click on that box is what turns the mode on and checks the
  mail the row stands for. The box is a click target on **every** row whether or
  not it is drawn (`ui.interact` over the avatar's slot, registered after the row
  so it is the topmost of the two), because egui hit-tests a pass against the
  widget rects of the previous one: a target that only appears once the pointer
  is already on it is a target a click can miss. Hovering the box is hovering the
  row — the row's fill follows `resp.hovered() || checkbox.hovered()` — so the
  box is a control inside the row and not a hole in it, and a click on it is
  never a click on the row: the row's own `clicked()` is false when the box was.
  The box is drawn by `paint_checkbox`: off is a `text_weak` outline that takes
  the accent under the pointer, on is an accent fill with the scheme's `on_accent`
  check on it, and a conversation with only part of its mail checked is the same
  fill with a dash rather than the check. A checked row takes the whole-row
  `selection` fill the mail open in the pane already has, so what is checked is
  visible past the 18 pt box; a partly checked conversation's own row does not,
  and says what it is with the dash alone.
  **Select mode also has a select-all row**, pinned under the title bar over the
  top of the list: the same box in the same column, a label that says what a
  click would do (`Select all` / `Clear selection`), and a hairline under it. It
  is a band of its own height (`SELECT_ALL_HEIGHT`, 32 pt), drawn *after* the
  rows the way the floating `Load older mail` button is (`select_all_band`, off
  the scroll area's own viewport): the list scrolls **under** it, so the one
  control that is about the whole list never scrolls out of reach. It is opaque
  — the list's own surface, filled edge to edge like a row — which is what the
  rows sliding under it need. The frame's top margin grows by its height while
  the mode is on, so at rest the first row
  starts below it and `virtual_rows` keeps its uniform-row arithmetic — the title
  bar's own padding stays, so mail still passes under the glass above the band.
  It is not the list header the column deliberately has none of — it exists only
  while the mode does. The whole band is one click target, unlike a mail row's
  box: there is nothing else a click on it could mean, so a mis-aimed click on
  the label must not be what leaves the mode. Its box says `CheckState` the way a
  conversation's does — off, the dash for part of the list, the check for all of
  it — and the label turns into the way out when the click would clear.
  **It is about the list and not about the rows**: the keys are every mail the
  folder holds — closed conversations included — or every search hit, built on
  the click and never per frame. The drawn state is one comparison, the
  selection's size against the list's, and that is exact only because the
  selection is always a subset of the list on screen: `cancel_select` on every
  list change as before, and two guards for the ways a checked key could leave
  the list without one — a fresh search answer drops the keys it no longer names
  (`Event::SearchResults`), and a UIDVALIDITY rotation drops its folder's
  (`Event::Messages`), since those UIDs are meaningless with the mail they
  named.
  What is checked is a `MailKey` — account, folder and uid — and not a uid, since
  a search lists hits from every folder and account at once. `select_mode` and
  `selected_mails` are in memory only, the way the open conversations are, and
  the column is a list *of* something: whatever moves it to another list — the
  navigation's folder click, the search popup applying a different query or
  Clearing it, an account added, edited onto another endpoint or removed — ends
  the mode and drops the checks (`cancel_select`), so nothing is ever checked
  that is not on screen. A conversation's own row stands for every mail it holds,
  so its box takes the whole thread in or out at once; checking a mail in the
  list *under* it is still one mail at a time, and the conversation's box then
  says `Partial` rather than lying about what is in.
  In select mode the mail actions' Delete is the only live cell
  (`mail_action_enabled`): reply, reply all, forward, label, flag and export are
  all about the one mail the reading pane shows, which is not what the click was
  about, so they are drawn dimmed with a tooltip saying so. Delete takes the
  selection — `delete_batches` groups the checked mail by account and folder,
  since a selection made over a search's hits spans folders, and each group is
  one `Job::DeleteMessage` on one connection — the rows go as the events come
  back, and a mail the server refused keeps both its copies and its check.
  Nothing about the mode is persisted, and there is no shift-click range and no
  Delete-key shortcut: a selection is made one checkbox at a time, or all at once
  with the select-all row.
- The window is undecorated on purpose (`with_decorations(false)` in
  `main.rs`): `titlebar_area()` in `app.rs` draws the custom title bar — an
  `egui::Area` fixed to the window's top edge and shown *after* the panels, not
  a `TopBottomPanel`, because the bar floats over the columns and the message
  list scrolls under it. At the top of the list it is the solid chrome band it
  has always been; once the list is scrolled it turns to glass — the fill keeps
  `GLASS_ALPHA` of the scheme's `panel`, the backdrop behind it is blurred
  (`src/glass.rs`) and a hairline appears along its bottom edge. One animated
  value (`TITLEBAR_FLOAT_TIME`) drives all three from the two content columns'
  scroll offsets (`KimoApp::list_scroll` and `mail_scroll`, each captured where
  its scroll area is built), so scrolling either of them back to the top docks
  the bar again rather than snapping it, and a column with nothing to scroll —
  or no list and no mail at all — leaves it docked. Inside
  the bar, left to right: the application icon (the brand, and the leftmost
  thing in the bar, kept `TITLEBAR_EDGE_MARGIN` clear of the window's edge
  rather than flush against it: the same
  embedded PNG the tray serves, downscaled once at startup because egui has
  no mipmaps to filter a 256 px source down to 18 pt), the nav collapse
  toggle, the compose button (the one mail action that needs no mail open, so
  it sits beside the toggle rather than in the group on the right, which acts
  on the mail in the reading pane; its glyph is a page with a pen writing on it
  — `paint_compose_icon`, one of the bar's own 24×24 outlines like the mail
  actions' below — where the icon set's pencil used to be. The page is a
  rounded-rectangle outline whose top-right corner is left *open*, and the pen
  is the straight stroke that crosses exactly there: no stroke of the page ever
  touches it, which is what says the pen is writing on the page rather than
  lying on top of it), the icon-only sync button (Debug mode only:
  Release syncs on its own), the search box (a field with the magnifier inside
  it, one widget,
  whose click opens the search popup), the group of mail actions
  (`titlebar_mail_actions`: reply, reply all, forward, label, flag, export,
  delete, one drawing function per glyph, each from a 24×24 outline through
  `glyph_point`
  and placed `MAIL_ACTION_PADDING` to the right of the search box), the
  preferences cog, drag area,
  double-click maximize, and the
  minimize/maximize/close buttons sent as `ViewportCommand`s. Every button in
  the bar is one hand-painted cell (`titlebar_button` + `titlebar_hover`): a
  `TITLEBAR_BUTTON_SIZE` (32 pt) square, centred in the bar's height,
  `TITLEBAR_BUTTON_GAP` (6 pt) from the next, flat at rest and filled under the
  pointer with a small rounded corner (`TITLEBAR_BUTTON_CORNER_RADIUS`, 6 pt)
  rather than the full-height square the bar's buttons used to fill. The left
  group, the mail actions and the window controls
  each set their row's `item_spacing.x` to that gap; the cells the bar places at
  exact x positions rather than in a row — the cog and the window controls —
  are filled through `titlebar_fill` instead. They are not `egui::Button`s for
  that reason — a stock button draws a card-colored frame with a border, which
  in a title bar reads as a widget among window chrome. Don't re-enable system
  decorations.
  The window is transparent (`with_transparent(true)` in `main.rs`) and its
  four corners are cut out of the frame by `gpu::CornerMask`: the last draw of
  a frame scales the four corner squares of the surface by the corner's
  coverage, through a `Zero`/`OneMinusSrcAlpha` blend on the colour *and* the
  alpha so that what is cut stays premultiplied, and the compositor shows what
  is behind the window there. That is why the surface is configured with
  `CompositeAlphaMode::PreMultiplied` — `Auto` resolves to `Opaque`, which
  would cut black corners instead — and why a surface with no alpha mode at all
  keeps its square corners rather than gaining black ones.
  The surface's *color* format is a gamma-space one (`Bgra8Unorm` / `Rgba8Unorm`,
  the first the surface offers), not the sRGB format `get_default_config` picks,
  and that is what makes egui's antialiasing come out right: egui blends
  premultiplied colors, and egui-wgpu's linear-framebuffer shader runs them
  through the sRGB curve *while they are still premultiplied*, so on an sRGB
  target every feathered edge — a rounded hover fill's corners most visibly —
  lands a shade darker than the shape it belongs to. egui-wgpu's own docs ask
  for a gamma-space target for this reason, and the scene texture, the composite
  and the blur all take their format from `config.format`, so nothing below
  converts anything either way. A surface that offers only sRGB formats says so
  once on stderr and keeps the dark edges.
  `WINDOW_CORNER_RADIUS` (8 pt) is also the radius a compositor-side window
  rule has to agree with (niri's `geometry-corner-radius`): without it the
  compositor still assumes a square window and its shadow shows through the
  cut corners.
  The mail actions are the bar's own cells, seven of them in a run: the same
  squares the rest of the bar's buttons are, laid out by a scope of their own
  whose `item_spacing.x` is `TITLEBAR_BUTTON_GAP`. The run's width is one
  function, `titlebar_cells_width`, which the fit plans the room with and the
  bar draws it with — the window controls are measured with the same function,
  so a cell cannot come out one size in one group and another size in the next.
  A glyph is `MAIL_ACTION_GLYPH` (18 pt) off its 24×24 outline, which leaves it
  7 pt of padding inside the cell. The run is deliberately not a framed group:
  the bar stays chrome plus flat icons, and the seven are held together by the
  clear chrome that sets them apart from the search box
  (`MAIL_ACTION_PADDING`) rather than by a pill. The seven enable and disable
  together, on `mail_action_enabled`: in view mode that is
  `has_selected_message` — the reading pane's own lookup, so a uid the pane no
  longer shows counts as no mail — while select mode leaves Delete alone live
  (see the select-mode bullet below), the one of the seven that has something to
  do with more than one mail. A disabled cell drops its glyph to
  `weak_text_color`, stops sensing clicks and never fills. Export and Delete
  are wired — they are the actions whose jobs exist so far — and the other four
  still run nothing, so a click on one of them is not even read.
  The bar is `TITLEBAR_HEIGHT` (40 pt) tall, its buttons
  `TITLEBAR_BUTTON_SIZE` (32 pt) squares centred in that, and the search box
  fills all but 16 pt of it (`TITLEBAR_SEARCH_HEIGHT`), with a rounder corner
  than the stock widget radius (`SEARCH_BOX_CORNER_RADIUS`); its frame is stock
  `TextEdit` chrome, so the radius is set on the widget visuals in a scope —
  and because the field is deliberately non-interactive, the slot that matters
  is `noninteractive` (egui picks the visuals by `Sense`, not by hover). Its
  *fill* goes to glass with the bar (`GLASS_FIELD_ALPHA`, animated by the same
  value): the one control in the bar with a background of its own would
  otherwise sit on the glass as a solid sticker, so it keeps only part of the
  scheme's `input` and the blurred mail shows through the box the way it
  shows through the bar, with the stroke left to say where the field is. That
  fill has to be set per widget (`TextEdit::background_color`) rather than on
  a widget slot, because the stock frame is drawn from
  `Visuals::text_edit_bg_color` (0.36's name for the slot; it is an `Option`
  now).
  The bar is laid out as *groups*, not as one run of widgets, because what a
  narrow window used to eat was the window controls: the row simply drew past
  the end of the bar, and the search box and the mail cells landed on
  minimize / maximize / close. `titlebar_rects` carves the bar into the room for
  its own content (`content`: everything left of the cog) and the fixed right end
  — the cog, a gap, the three window controls, ending `TITLEBAR_EDGE_MARGIN`
  short of the window's edge, the count coming from `titlebar`'s own button
  list so the two cannot drift — and `titlebar_fit` decides what the room
  between them holds. Those two rects are a cell's own band rather than the
  bar's full height, since a button is 32 pt in a 40 pt bar; `content` is the
  one rect that is the bar's own shape. Every group is
  drawn in a rect of its own, its own `UiBuilder::max_rect` and its own
  `clip_to` for the clip, and that is the rule the rest rests on: a group the bar
  cannot fit is cut off at its edge rather than drawn over its neighbour, and,
  since egui hit-tests a widget through its clip rect, a click past that edge
  belongs to the group next to it instead of to a widget that is not really
  there. `nothing_in_the_title_bar_is_drawn_into_the_window_controls` holds the
  drawing to it by sorting a frame's shapes by the clip they were drawn through,
  which is what tells the bar's right end from a widget that has wandered into
  it — what the old bar did at every width under about 970 pt.
  The room is given up in one order (`titlebar_fit`, pinned by
  `the_title_bar_gives_its_room_up_in_one_order`): the clear chrome in front of
  the mail actions closes from `MAIL_ACTION_PADDING` down to
  `MAIL_ACTION_MIN_PADDING` first, then the actions leave — as the one group
  they are, because half a run of seven reads as a mistake rather than as a
  smaller bar — and only then does the search box narrow, down to
  `TITLEBAR_SEARCH_FLOOR` and no further, with the box itself the last thing to
  go. The window's own buttons are what the bar keeps to the end, and every width
  is monotone in the room, so resizing a window never flickers between two
  shapes. The account status strip is right-aligned against the cog rather than
  run after the actions, so a spinner appearing does not move the search box, and
  the width it measures is what the middle group is fitted around; the actions
  are placed at an exact x rather than by the row's item spacing, so the gap in
  front of them is the fit's and not a leftover; and where the box really ended
  (not where the arithmetic expected it to) is where they start, with the group
  simply not drawn if the room has run out by then. The fit and the widget also
  have to agree on what "the box's width" means: `TextEdit::desired_width` is the
  *whole* box in 0.36, the margins coming out of it rather than being added to
  it, which is why the rect the widget hands back — the frame, not the inner text
  rect — is used as the box as it is. Re-adding those margins, as the 0.31-era
  code did, puts the magnifier 22 pt left of the box, over the compose cell, and
  stretches the click target over it. The test named
  `the_search_box_is_as_wide_as_the_fit_planned` is what keeps the fit's number
  and the widget's the same one.
- The frosted backdrop is hand-written GPU code (`src/glass.rs`), because egui
  has no blur: nothing in the painter can sample what has already been drawn.
  On wgpu that is a decision about the *frame*, not something a paint callback
  can do on its own: a texture cannot be sampled by the pass that is drawing
  it, and the pass's target here is the window's surface, which is not even
  created with `COPY_SRC`. So the frame is split around the glass —
  `crate::gpu` renders everything *behind* the bar into an offscreen scene
  texture, `Glass::blur` copies the bar's strip of it into a texture of the
  strip's own size (a texture-to-texture copy, the one thing wgpu allows) and
  blurs that in two passes, and the second half of the frame — the bar's fill,
  its icons, its text — is drawn onto the surface over the blurred strip. Where
  the split falls is egui's own shape list: the glass callback is the first
  shape that belongs to the second half, and the callback is also what draws
  the strip, into the viewport egui has already set to the bar.
  The blur is separable because a single two-dimensional kernel cheap enough to
  run per frame is sampled too sparsely and leaves a comb of aliasing over the
  very letters it is meant to destroy; seventeen taps a pixel and a half apart,
  twice, cost less and dissolve the text instead. One pipeline does all three
  draws, since with a step of zero the blur is a plain copy. Every failure is
  soft: a device that will not take the pipeline or the textures reports once,
  sets `failed`, and leaves the bar translucent over the crisp content. There
  is no multisampling to fight here — this integration asks for none — which is
  what the copy wants anyway.
- Deleting mail (the title bar's Delete → `KimoApp::delete_selected_message` →
  `Job::DeleteMessage { uids }`) is **remote first, local after**, one mail at a
  time: the worker asks the server to drop each message and only then deletes
  that database row (`db::delete_message`), so mail that is still on the server
  never disappears from here and a deletion that fails — no connection, a
  read-only mailbox — leaves that mail's copies alone and reports why on the
  account (`Event::SyncFailed`, prefixed "delete:"). The list of uids is what the
  button was about: one mail from the reading pane, or every mail checked in
  select mode. A run is one connection, not one per mail — IMAP checks LIST and
  SELECTs once and then probes, flags and expunges each uid
  (`ImapClient::delete_messages`), POP3 is one session whose `QUIT` commits every
  `DELE` — and a uid the server refuses is that uid's alone: the rest of the run
  is still asked, each mail that went comes back as its own
  `Event::MessageDeleted`, and the first refusal is what the account carries.
  The reverse case is the other half of the rule: a mail
  the server no longer has is deleted **locally only**, without a command that
  could touch anything else. IMAP asks before it flags — the mailbox has to
  still be in LIST (a mailbox that *is* listed but refuses SELECT is an error,
  not a licence to drop the local copy) and then `UID FETCH <uid> (UID)` has to
  answer for the UID — and only then is it flagged `\Deleted` and expunged,
  with `UID EXPUNGE` naming the one message and the mailbox-wide `EXPUNGE`
  only as the fallback for a server without UIDPLUS (which answers BAD). POP3
  has no folders and no flags: the UIDL is the whole address of the message,
  `DELE <number>` goes out for it when the drop still answers for it, and the
  `QUIT` that `pop3_delete` always sends is what commits the deletion.
  The row leaves the list only when the job reports success
  (`Event::MessageDeleted`) — the job brackets its round trip with
  `SyncStarted`/`SyncDone`, so the account's spinner is what says the mail is
  on its way out — and that event is also where the reading pane is cleared if
  it was showing that mail, its cached Blitz pages go (`forget_blitz_message`)
  and an active search is asked again, since the hit list is a snapshot of the
  database. A half-finished delete is harmless: a mail whose remote copy went
  but whose local row survived is deleted again by the same button, which then
  finds nothing on the server and stops at the local half.
  **Deleting is permanent.** There is no Trash: IMAP expunges and POP3's `DELE`
  is the server's own hard delete, so the button does not ask first — the
  tooltip says what it does — and nothing in the app can undo it. Deleting to
  a Trash folder instead (IMAP `UID COPY` + expunge; POP3 cannot) is a decision
  for the project owner, not a fix to slip in. The folder's sync mark is
  deliberately not moved back: the mail is gone from the server too, and a mark
  that walked back to it would only make the next sync fetch it again.
- Exporting a mail (the title bar's Export → `KimoApp::export_selected_message`
  → `Job::ExportMessage`) is the one thing in the app that writes a mail out of
  it. A worker fetches the message's original RFC 5322 source from the server —
  IMAP `UID FETCH <uid> (UID FLAGS BODY.PEEK[])` through
  `ImapClient::uid_body` (PEEK on purpose: fetching a mail to export it must not
  mark it read), POP3 `UIDL` + `RETR` through `Pop3Client::retr_by_uidl` — and
  writes it byte for byte to `$HOME/Downloads/<subject>.eml`
  (`storage::downloads_dir`, made on the first export). There is no file dialog:
  one destination — the same directory a downloaded attachment goes to, see the
  attachment-card bullet — and the app says the path it wrote.
  **The original is the point.** What is stored here is the parsed mail, with
  each attachment's name, type and size but never its bytes, so a `.eml`
  rebuilt from the database would not be the message the sender wrote, while
  this one is the message whole, attachments and all. The price is that mail the server no longer has cannot be exported at
  all, and says so rather than writing something close (`Event::SyncFailed`,
  prefixed "export:" — the local copy is not a substitute for the original).
  The file is named after the subject, with what a file name cannot carry
  replaced and the whole capped at `EXPORT_STEM_BYTES` (`export_stem`; `slug`
  would ASCII-fy every non-Latin subject away), numbered `<stem>-2.eml` and on
  when a name is taken, and *created* (`create_new`) rather than merely asked
  about, so two exports of the same mail cannot overwrite each other. The job
  brackets its round trip with `SyncStarted`/`SyncDone` like a delete, and
  answers `Event::MessageExported { path }` — with the path, because a name that
  was taken is numbered and only the worker knows what it got. The app shows it
  as one line above the message list for `NOTICE_TIME` (`list_banner`, the
  place the storage warning stands) — the same line a finished send uses, which
  is why it is named for what it is rather than for the export.
- Attachments are cards under the body (`attachment_card`), one per part the
  mail named a file for, in the order the parts appear in it: the page glyph,
  the file name, and — under the name — the MIME type and the size, whichever
  of the two the mail was fetched with (`attachment_meta`; `human_size` counts
  in binary units, the way a file manager on this desktop does, and drops a
  `.0`). **The cards are one run, not one per line**: each as wide as the name
  and the type under it want — `ATTACHMENT_CARD_MIN_WIDTH` at the floor, the
  mail's own column at the ceiling — laid across that column and wrapped onto
  the next row where the next one does not fit (`horizontal_wrapped`,
  `ATTACHMENT_CARD_GAP` both ways), so a mail with ten parts is a block of cards
  rather than ten lines, and a pane too narrow for the run wraps instead of
  cutting it. **A name is drawn whole** — the card is measured from it
  (`layout_no_wrap`, `ATTACHMENT_NAME_FONT_SIZE`) rather than given a width to
  be cut down to, and only a name the column itself cannot hold falls back to
  `truncate_galley` with the tooltip carrying the whole of it. Both of a card's
  lines are smaller than the body text (11 pt and 9 pt): a card is a note about
  the mail, not part of it. The run is laid out in a scope of its own
  — the column the headers were drawn across, taken from the pane *before* the
  body is drawn — because a body wider than that column (a fixed-width HTML
  table) widens the ui as it lays itself out, and the cards belong to the mail,
  not to what the mail overflowed by. They are the
  last thing on the mail's card whichever renderer drew the body,
  because `mail_card` draws them after `mail_body` rather than inside it: a
  Blitz body is a raster a worker made, and the cards are not part of it.
  **The whole card is a link, and the name is drawn as one**: the scheme's link
  color, a line under it while the pointer is on the card, the card filled with
  the hover color and the pointer turned into a hand. A click is a download —
  `KimoApp::download_attachment` → `Job::DownloadAttachment` — and it is the
  round trip an export makes, one step further: what the database holds is the
  name, the type and the size (schema v6), never the bytes, so the worker fetches
  the mail's original source, takes the part out of
  it (`mail::attachment_bytes`) and writes the file into
  `storage::downloads_dir` — the same directory, the same one-line
  `Saved to <path>` notice and the same `create_new` numbering as an export,
  through the one writer both use (`write_file`; the download's own rules are
  `download_name`, which keeps the sender's file name, its last path component
  and a plausible extension, and `safe_stem` behind both).
  The click carries the mail, the part's *place* in the mail and the name the
  card showed; the worker re-parses the original and refuses unless both match
  — handing over the bytes of some other part is worse than refusing — and the
  refusal is the account's, prefixed "attachment:", the way "export:" and
  "delete:" are. Nothing is kept afterwards either: the bytes exist in the
  downloads directory and nowhere else, and mail the server no longer has cannot
  be downloaded at all, and says so rather than writing something close.
  The hover text shows the whole name where the column had to elide it, and the
  type and size under it. A part with no file name is not an attachment at all
  (`mail::collect_attachments`), and a mail cached before v6 is the name alone:
  no second line, rather than a separator with nothing after it.
- The compose window (`compose_window`, opened by the title bar's pen) is an
  `egui::Modal`, like the search popup and Preferences: a card `COMPOSE_WIDTH`
  wide in the foreground layer, whose backdrop dims the columns and the bar and
  takes their clicks, so a mail is written with the rest of the app out of the
  way. There is one of it — `KimoApp::compose` holds the draft, and the button
  leaves an open card alone rather than resetting what has been typed into it.
  From is the configured accounts, defaulting to the one whose mail is open and
  falling back to the first (`compose_from`); To, Cc, Bcc, Subject and the body
  are plain strings in `ComposeDraft`, deliberately not widget state: a card
  opened again must start empty. The boxes are a fixed label column plus what
  `COMPOSE_WIDTH` leaves, the body is `COMPOSE_BODY_HEIGHT` tall and scrolls
  inside that rather than growing the card, and the first frame puts the
  keyboard in To.
  **The backdrop does not close it.** `ModalResponse::should_close` — a click
  beside the card, or Escape — is deliberately ignored: the draft is the only
  copy of the mail, and neither should be what throws it away. Discard is the
  way out. The app's other two modals do close on the backdrop; the difference
  here is the draft.
  **Send is wired, and the card owns everything about it.** One rule decides
  whether the mail can go out — `send_blocker`: an account, one that has an
  outgoing server, a username that is an address, at least one recipient, and
  every entry in To/Cc/Bcc an address (`outgoing::parse_addresses` is what
  unwraps a `Name <addr>` and refuses a typed CR/LF before it could become a
  header of its own). The footer shows that reason, the `Sending…` state while
  a worker has the mail, or the server's own refusal — and the worker checks
  the same rules again in `sync::send_message`, because nothing leaves the
  machine as mail on the strength of a frame having said it could. While a send
  is in flight the whole card is disabled, Discard included: a draft that is
  already on its way out is not one to edit or drop.
  **Nothing is persisted.** There is no drafts folder and no outbox, so Discard
  is the end of the mail — and until `Event::MessageSent` arrives, the card is
  the only copy of it, which is exactly why that event is what closes it. A
  refusal keeps the card (and the draft) with the server's words in it, ready
  to be sent again. Adding a way to keep a draft, attach a file or queue a send
  is a decision for the project owner, not an oversight to fix in passing.
- **The automatic sync is a preference, and the working mode is its outer
  switch**: `auto_sync` (off unless asked for) sweeps *every* account every
  `auto_sync_minutes` (15 by default, clamped to 1–1440) — `KimoApp::sync_all`,
  the same sweep the title-bar Sync runs, so an account that is not configured
  is not asked and a failed one reports itself like any other sync. It runs
  only while `WorkingMode::syncs_automatically()` holds: a Debug session never
  syncs on a clock, and the preferences page disables the two controls and says
  so rather than leaving a switch that does nothing. `pump_auto_sync` is the
  clock, called once a frame: `auto_sync_wait` is the whole decision (nothing
  while off, otherwise the interval left since the last sweep, never negative),
  and the wait it returns is what the app asks egui to repaint after — no other
  timer runs, and a frame with nothing to draw would otherwise never come. A
  sweep already in flight is left to finish and the clock is set anyway, so a
  busy account is asked again a full interval later instead of every frame; the
  clock is reset by `sync_all` itself, so pressing Sync is not a reason to be
  swept a moment later. Changing the preference restarts the clock, and the
  preference is what `settings.json` keeps — the timer is not persisted and
  starts fresh with the process.
- `preferences_modal()` is the one place to configure Kimo: an
  `egui::Modal` (backdrop click or Escape closes it) with a sidebar of
  pages — **General** (the working mode and the automatic sync), **Accounts**
  (account list with
  use/edit/remove, and the add/edit account form in place of the list: it is
  pre-filled from the account it was opened on, and `Save & sync` updates that
  account and syncs it) and **Appearance** (theme, Simple/Blitz HTML
  renderer, the interface/fixed-width fonts — all persisted in
  `settings.json` — plus a read-only preview of the palette the theme
  resolves to). Configuration belongs on a page there, not as another
  title-bar toggle; the title bar keeps only actions (syncing, window
  controls), and switching accounts is a navigation-column affair.
- Colors live in one place, `src/theme.rs`. `Theme` (system/light/dark — the
  only part that is persisted) resolves to a `ColorScheme`, and both defaults
  are filled in slot by slot, `ColorScheme::light()` and
  `ColorScheme::dark()`: surfaces (panel — the chrome; content — the message
  list and the reading pane's card; window, card, hover, input, code, border;
  and `row_edge` / `row_edge_top`, the two faint lines a mail row is drawn
  with, fainter than a hairline and the top one fainter than the sides),
  text and weak text, accent / selection / link, warn / danger / danger_hover,
  and the avatar palette — shared by both themes on purpose, so that a sender
  keeps its color when the theme changes. `content` is the lighter of the two
  surfaces in both schemes, and the step between it and `panel` is the *only*
  thing that draws the card, its gutter and the list's hairlines — the dark
  scheme used to set both slots to the same color, which is a reading pane with
  no card in it, so `theme.rs`'s tests pin a minimum ratio between them.
  `ColorScheme::visuals()` maps the scheme onto egui's `Visuals` for every
  stock widget, while `theme::install` also parks the scheme in the context,
  where the hand-painted UI (title bar, folder rows, message cards, avatars,
  spinners) reads it back through `theme::of`. So a color is added to the
  scheme and nowhere else — no literals in `app.rs` — and stock-widget slots
  stay reachable through egui itself (`ui.visuals().error_fg_color` is the
  scheme's `danger`, `widgets.active.bg_fill` its `selection`). egui derives
  its own weak/strong text variants by tinting the body color towards the
  panel, so `RichText::weak()` is close to, but not the same as, the scheme's
  `text_weak`; the folder and message rows use the latter. Preferences →
  Appearance previews the resolved scheme (`ColorScheme::swatches`,
  `theme::hex`) but the palette is not editable yet, which is why nothing
  about it is written back to `settings.json`. `theme.rs`'s tests pin the
  contrast floors (body text ≥ 4.5:1 on its panel *and* on its content, accent
  ≥ 3:1, text on the selection and hover fills ≥ 4.5:1, the close glyph on its
  red ≥ 4.5:1, both schemes' surfaces on opposite sides of the brightness
  scale, and the mail rows' edges fainter against the list than a hairline,
  with the top edge fainter than the sides); keep them green when you touch a
  color.
- Fonts are discovered, never fetched through fontconfig (hard rule 1):
  `fonts::scan()` walks the usual directories and reads each face's `name`
  table with ttf-parser. That took ~210 ms for the ~1000 installed faces on
  the development machine, so it runs as `Job::ScanFonts` on a worker thread
  and arrives as `Event::FontsScanned`; only the single-file read that
  installs a picked family happens on the UI thread. Families are keyed by
  their *typographic* family name, and each one keeps its plainest face
  (regular weight, non-italic) so "DejaVu Sans" does not resolve to the
  ExtraLight file. `fonts::install` also binds the named families
  `kimo_ui`/`kimo_mono` — epaint panics on an unbound named family — which
  the Appearance page previews through. Blitz keeps its own curated font set
  (`blitz_render::load_fonts`); the Preferences choice applies to the egui UI
  and to monospace text in the built-in HTML renderer. The CJK fallback is a
  pan-CJK collection (Noto Sans CJK), whose faces cover the same characters
  but not the same *forms*: the face therefore follows the locale
  (`CjkScript`: zh-CN → SC, zh-TW/HK → TC, ja → JP, ko → KR, anything else
  → SC), because taking the collection's first face means the Japanese one —
  which is how Chinese mail came out with Japanese glyphs for 复 and friends.
  Do not go back to an index-free `read(path)`.
- The navigation column is where accounts are switched, which is why the
  title bar has no account menu: every account is a collapsible section
  (`collapsed_accounts` holds the collapsed ids, in memory only), whose header
  shows the name and the account's unread total and whose folders are indented
  under it. An account with no cached folder at all still offers INBOX, since
  clicking it is what starts the first fetch. Selecting a folder of another
  account is what makes it the active one.
  What it draws is a cache of the display model, the way the reading pane
  caches the parsed body: `AccountUiState::nav` derives the rows (`FolderRows`)
  from the account's folder list and mail only when `folders_rev` moved, so a
  frame costs the drawing rather than a decode, a `folders::kind_of` and an
  unread count per folder. The revision moves when a `LIST` named **different**
  mailboxes (`Event::Folders` compares before it replaces), when mail arrives or
  leaves (`Event::Messages`, `Event::MessageDeleted`) and when a mail is read
  (`open_message` marks it seen locally) — so a sync that found the same folders
  leaves the column exactly as it was, and a change is a rebuild of the rows
  rather than a redraw that happens to look the same. Rows come in the two
  groups the folder list defines: first the mailboxes the server lists — in
  `imap::order_mailboxes` order, which is the order a `LIST` answer is given in
  and the order the cached list is drawn in, so a sync does not reorder the
  column — and then the ones it does not, which are here for the mail cached
  under them, drawn weak whatever their unread count with a tooltip saying why,
  and still clickable. A folder of that second kind is only dropped from the
  column when it holds no mail at all; its row in the database stays.
- Folders are custom rows — kind icon, name (elided with an ellipsis), unread
  count right-aligned — because the whole row should be the click target and
  carry the selection highlight, which `selectable_label` cannot do once an
  icon sits in front of the text. The icon comes from `folders::kind_of`,
  which classifies the *decoded* mailbox name: an exact keyword first (`INBOX`, `Sent`, `Trash`, `归档`), then
  keyword plus a standard ending (`已删除邮件` = 已删除 + 邮件,
  `Sent Items` = Sent + Items), walking path segments from the right so
  `[Gmail]/All Mail` and `项目/已发送/2026` match as well. Only those
  endings, so `Sentinel` and `已发送的报表` stay ordinary folders. The
  Chinese names are in the table because mailbox names arrive in the server's
  language — that is data, not UI copy, and not an i18n framework (rule 6).
  SPECIAL-USE attributes are still not read (the client never asks for the
  capability), so the name is the only source of a kind. Row icons live in
  `assets/folder-icons/`: white on transparency, 48 px, baked from the SVGs
  in `assets/icons/` with `sed 's/currentColor/#ffffff/g'` plus
  `rsvg-convert -w 48 -h 48`, and tinted per row. White matters because egui
  tints by multiplying (a black bake could never take the text color), and
  `folders.rs`'s test pins the size and the white/transparent pixels.
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
- When touching the UI, actually run `cargo run` and click through it; the
  layout has no golden tests. `blitz_render` has a developer tool for
  eyeballing fidelity without the GUI:
  `KIMO_RENDER_HTML=mail.html cargo test -- --ignored dump_render` writes
  `mail.png` next to the input. For cost rather than fidelity, use
  `render_bench` (test-only, asserts nothing):
  `cargo test --release -- --ignored --nocapture bench_render` times both
  renderers on a short mail, a newsletter-shaped fixture and — with
  `KIMO_BENCH_DB=~/.local/share/kimo/mail.db` — the median-sized and largest
  HTML bodies in the database. Note what the numbers mean: the built-in
  renderer lays the body out on *every* frame the reading pane is visible —
  there is no cache of the widget tree — but it parses it once and keeps the
  width it settled at (`BodyCache`), so a frame costs the layout, not the parse
  and not a reflow per pixel of a splitter drag. Blitz pays parse, layout and
  rasterization once per (message, pane width), on a worker thread. For whole
  frames rather than bodies — including what a splitter drag costs — `app.rs`
  has `bench_frame_cost`, which drives the real app headlessly at a fixed
  splitter position and while it is being dragged:
  `cargo test --release -- --ignored --nocapture bench_frame_cost`. Its
  companion `bench_nav_slide` measures those frames through the navigation
  column's collapse and its slide back open, which moves the message column's
  width the way a drag does
  (`cargo test --release -- --ignored --nocapture bench_nav_slide`).
- Blitz is built with `blitz-dom`'s default features **off**: its
  `system-fonts` feature would link fontconfig (a C library, hard rule 1).
  `blitz_render` therefore registers fonts from disk itself (a curated set —
  parsing every installed face took ~46s in a debug build) and maps the CSS
  generic families plus the per-script fallbacks onto them. Keep it that way,
  and keep the curated list in sync with the generic families.
- Blitz's painter scale must equal the document's viewport scale. Text is
  laid out at the viewport scale but positioned with the painter's scale, so
  `paint_scene(.., scale, ..)` with a scale the document was not resolved for
  draws oversized text that overflows its box — and, because the reading pane
  sizes tiles from the page's own scale, a mismatch also mis-sizes the whole
  page in the pane. To rasterize more cheaply, re-layout via `set_viewport`
  (see `blitz_render::render`) and report the scale you used in `Page::scale`.
- Keep `complex-scripts` on blitz-dom. Without it parley has no CJK/Thai/
  Khmer/Lao segmentation model: ICU logs "No segmentation model for complex
  script" into stderr for every such run and the text is only broken by the
  coarse UAX-14 rules. The dictionaries it enables are already-compiled ICU4X
  data, so the feature costs no new crates.

## Accepted limitations / roadmap

Do not "fix" these casually — they are known:

- Downloading an attachment is a round trip to the server, never a read of the
  cache: Kimo keeps the name, the MIME type and the decoded size and never the
  bytes, so a card's link fetches the mail's original source again and there is
  nothing to hand over for mail the server no longer has. Nothing is opened
  afterwards either — the file lands in `~/Downloads` and the desktop's own
  programs are what open it. Export remains the way to get the whole message,
  attachments included, as one `.eml`.
- Threading is per folder and, for mail cached before schema v7 — or mail whose
  sender wrote no ids at all — by subject: a reply joins the subject it answers,
  but a conversation whose mails were filed into different folders (or into and
  out of a Trash) is more than one conversation here. Search results are a flat
  list of hits and are not grouped at all, and the reading pane shows a
  conversation only while the list is threading: with the preference off, a
  click opens the one mail it always did.
- TLS finite-field DH covers the 2048-bit groups listed in
  `src/protocol/ffdhe.rs` and nothing else. A server whose DHE group is
  another size, or yet another custom prime, still fails the handshake with
  `SelectedUnofferedKxGroup`; see the FFDHE convention above for what to add.
- Deleting is permanent: there is no Trash folder and no undo, and a click on
  the title bar's Delete expunges the mail on the server without asking first —
  the one the reading pane shows, or a whole select-mode selection, which is
  just as final. There is no Delete-key shortcut and no shift-click range: a
  selection is made one checkbox at a time, or every mail of the list at once
  (see the select-mode bullet under Conventions) — and that click is just as
  final, as is a row's own hover button (see the message-row bullet).
- A written mail goes out over SMTP and no further: no attachments (there is
  nothing to pick a file with — a native dialog is a dependency, and a portal on
  Wayland), no HTML body (the card is a text box, so the mail is `text/plain;
  charset=utf-8`, base64-encoded), no drafts — closing the card is the end of
  the mail, and a failed send keeps the draft in the card but nowhere else — no
  outbox, queue or retry, no DKIM, and no OAuth2: submission authenticates with
  `AUTH PLAIN`/`AUTH LOGIN` over TLS, or not at all when the server asks for no
  login. The title bar's reply / reply all / forward / label / flag buttons are
  still drawn but not wired to anything (export, delete and the compose pen are
  wired; see those bullets above).
- Credentials in plain text in `mail.db` (mode 0600, see above); no
  OAuth2/XOAUTH2. The database is not encrypted, and the app never renames or
  deletes it on its own: a corrupt file is the user's to move aside.
- HTML mail renders through the in-egui subset renderer (`src/html.rs`).
  CSS is the `src/css.rs` subset (colors, fonts, sizes, display/visibility);
  compound selectors and layout CSS are ignored. Simple tables render as
  aligned grids; layout tables (block elements inside cells — most HTML
  mail) render as flowing full-width rows, because egui Grid sizes columns
  to the minimum wrapped width. `cid:` inline images render from the
  attachment bytes stored on the message (`InlineImage.data`, the
  `inline_images` table). Improving fidelity here means improving this renderer — Blitz mode
  is the escape hatch for mail this subset cannot lay out, not a replacement.
- Blitz renders tables in the *separate* borders model: `blitz_render`'s
  `COMPAT_CSS` overrides `border-collapse` because blitz's collapsing paints
  the first cell's border width/colour across the whole table — a 3px dark
  frame around every `border-collapse: collapse` table whose first cell
  declares no border, and wrong-coloured cell borders. Consequence: adjacent
  cell borders do not merge (a 1px border on both sides of a shared edge draws
  2px), and `cellspacing` stays 0 unless the mail sets `border-spacing` in
  CSS. Fixing this properly means fixing blitz, not this workaround.
- Blitz mode is a raster: text is not selectable, links are clickable boxes
  (from `get_client_bounding_rect`), form controls are inert, and mail with a
  fixed-width layout is laid out at its own width and scaled down into the
  pane. The finished page is recorded once as an anyrender `Scene` and
  rasterized one 2048 px slice at a time, so page height is not limited by a
  single surface (`Scene` + `append_scene` with a translation; see
  `blitz_render::render`). A page beyond the 40 MP budget (160 MB of textures)
  is re-laid out at a smaller scale — see the paint-scale convention above —
  and only a page too tall even for the 0.5× floor is cut, which sets
  `Page::truncated` and shows a note in the pane. The reading pane also
  scrolls back to the top whenever another message is opened. Remote
  resources are fetched during the render, so the first view of an
  image-heavy mail can take a few seconds (the disk cache makes the second
  one fast). Fonts are the curated disk set, not fontconfig's.
- POP3 changes one thing server-side — the `DELE` behind the Delete button —
  and a sync does not notice deletions either: mail removed in another client stays in the database
  and in the list forever. What a sync does is incremental by UIDL — a message
  already stored is never downloaded again — and bounded below by the
  `first_num` floor, so the messages a first sync skipped arrive only when
  `Load older mail` asks for them.
- Flags of mail that is already stored are not read back from the server. IMAP
  `\Seen` is taken for a message as it is fetched, and from then on the local
  flag is the only truth (`seen` only ever grows). Sync used to refresh the
  newest batch for free by refetching it; now that a warm sync fetches nothing
  it already has, mail read in another client keeps the unread mark it had here.
- No IDLE/push for IMAP; sync is manual or on-startup. A desktop
  notification is therefore only ever raised by a sync that ran, and never for
  mail that arrived while the app was closed until the next one asks for it:
  `Job::Sync` at startup, the title-bar button, the automatic sync, or the
  first click on a folder. A notification does nothing when it is clicked —
  the default action would mean listening for the daemon's `ActionInvoked`
  signal, which this client does not do — and its icon is scaled for the
  monitor the window is on, so a notification a multi-monitor daemon draws on
  another screen keeps that scale.
