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
  theme-following, selectable, clickable, no layout fidelity.  The mail's own
  colours are read as CSS (`csscolorparser`) and adapted to the theme in force
  (`palette`, OKLCH) before they are drawn — see the mail-colour bullet under
  Conventions.  *Blitz* feeds
  the same HTML to the pure-Rust Blitz browser engine (stylo/taffy/
  vello_cpu) in `src/blitz_render.rs` and blits the rasterized page — close
  to what the sender designed, but a static image with clickable link boxes.
  Remote images and stylesheets load **by default** in both (see the
  conventions below). There is still no JavaScript.
- **Charsets:** mail-parser is built with the `full_encoding` feature
  (pulling in `encoding_rs`, pure Rust) so GB2312/GBK/GB18030/Big5/
  Shift-JIS/… mail decodes to proper UTF-8 instead of Latin-1 mojibake.
- **The file dialog is the desktop's, not Kimo's.** There is no file-picker
  crate: the dialog is asked for over D-Bus from the compositor's own portal
  (`src/portal.rs`, `org.freedesktop.portal.FileChooser`), which keeps the
  dependency list as it is — no GTK, no portal crate with an async runtime behind
  it — and is the only thing that works under Wayland, where winit 0.30 delivers
  no drag-and-drop events at all. Two calls are made: `OpenFile` for the files an
  attachment is chosen with, and `SaveFile` for where an export goes — the save
  told `current_name` and `current_folder`, the name and the folder an export
  would have used itself, so that pressing Save without touching anything gets the
  file a session with no dialog gets. A chooser is a `Job` because the call does
  not return until the user has answered, and the answer arrives as a `Response`
  signal on the request object the reply named (read in whichever order the two
  come: a portal that refuses at once sends the signal first). Two things about
  the wire that cost a debugging session and are worth keeping: **the connection
  greets the bus (`Hello`) before anything else**, because a bus answers anything
  sent before the greeting with an error and then *closes the connection* — which
  arrives as "bus closed the connection" and looks nothing like a missing
  greeting; and **the methods have been `OpenFile` and `SaveFile` for as long
  as the interface has had options** — every release back to 0.10 (2018) declares
  those two and not the bare `Open`/`Save` one might expect, so a refusal means
  "this desktop has no dialog" and is not retried under another name. The token
  that becomes the last element of the request object's path counts rather than
  naming the process alone, because two dialogs can be up at once — an
  attachment's and an export's — and two requests under one token would be two
  requests for one object path. The developer test
  (`cargo test -- --ignored the_desktop_answers_a_chooser_call`) walks both calls
  against the real portal on a session that has one: it opens each dialog and
  closes the request the moment the reply names it.
- **Sending is SMTP submission.** The compose card's Send hands its draft to
  a worker (`Job::SendMessage`), which speaks `EHLO`/`STARTTLS`/`AUTH`/`MAIL
  FROM`/`RCPT TO`/`DATA` through `src/protocol/smtp.rs`, and the message itself
  is built by `src/outgoing.rs`. A copy goes into the account's Sent mailbox
  over IMAP when the server has one. A mail with files on it goes out as
  `multipart/mixed` — the text is one part and each file another, all base64 —
  and the files are read by the worker as it sends, never by the UI thread. There
  is no drafts folder and no outbox — see hard rule 4 and the compose-window
  convention below.
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
  one popup and not forty. A finished **export** raises one of its own
  (`exported_notification`: "Mail exported" / "Saved to <path>"), which is
  where that answer lives rather than above the message list — the click was in
  the title bar and the file it wrote is not in the list. Preferences → General
  has the switch for *new mail* (`notifications` in `settings.json`, absent
  meaning on), and an export is deliberately not gated by it: the switch is
  about mail that arrives by itself, while an export answers a click the user
  has just made, and a click answered with silence is worse than one answered
  where they are not looking. A session with no notification daemon is simply
  quiet. `cargo test notify_probe -- --ignored` under `dbus-run-session` with
  `dbus-monitor` watching is how the wire form is checked by hand.
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
   (nothing about a draft is persisted), an outbox/queue or any retry, HTML
   bodies, DKIM or any signing, and OAuth2/XOAUTH2 — SMTP
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
src/portal.rs           the desktop's own file dialog, asked for over D-Bus (XDG portal)
src/mail.rs             raw RFC 5322 bytes → MailMessage (mail-parser; HTML kept, attachments
                        as name/type/size, one part back out for a download)
src/outgoing.rs         the mail being sent: addresses, headers, encoding → RFC 5322 bytes
src/html.rs             HTML mail → egui widgets (subset: blocks, lists, tables, links, inline
                        styles; the mail's colors adapted to the theme)
src/blitz_render.rs     HTML mail → rasterized page via Blitz (headless render, net provider, fonts)
src/css.rs              tiny CSS subset (decls, tag/class/id selectors, CSS colors) for HTML mail
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
  raised from (see the notifications bullet above). **Syncing is the app's own
  business, and there is no mode that switches it off**: the startup sweep and
  the fetch-on-first-open always happen, and the title bar's Sync cell — always
  there, beside the cog — asks for the same sweep at any time. It sweeps every
  account and, for the one whose folder is open, fetches that folder even when
  it has no mark: the column says "Not fetched yet — click Sync in the title
  bar" for exactly that folder, and a folder with no mark is not part of a
  sweep — `sync_queue` is what puts it first, so the message is not a lie.
  The only sync nobody presses for is the clock in the automatic-sync bullet
  below, and that is a preference which is off unless it is asked for.
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
- Remote mutations are `UID STORE +FLAGS.SILENT (\Seen)` (IMAP), `UID STORE
  ±FLAGS.SILENT (\Flagged)` for the reader's flag (IMAP too; a POP3 account has no
  server-side flags and keeps the flag locally), and, behind
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
  meant one — every mail in it starts unlabelled.
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
  `egui`), `threaded` (whether the message list groups mail into
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
- **The columns give way to a narrowing window in one order**
  (`plan_columns`, read by `KimoApp::ui` every frame): the navigation
  collapses first, then the reading pane is capped at what leaves the list
  its `LIST_MIN_WIDTH` (240 pt), then the pane stops being a column and
  becomes a popup over the list, and the list itself never narrows past its
  minimum — a window narrower than that scrolls it sideways instead
  (`mail_list` switches the scroll area to both axes, and the rows keep
  their minimum width through `virtual_rows`, which lays them out at the
  union of the viewport and the content's own min rect). The thresholds are
  the columns' own widths rather than constants: the navigation collapses
  on crossing below navigation + pane + the list's minimum, where the two
  widths are what the splitters left the panels at (egui's `PanelState`,
  read through `panel_width`). The pane's cap is placed by writing that
  same state (`set_panel_width`) rather than only clamping it, so widening
  through the cap follows the window back up; and the remembered width to
  restore to is the app's own (`pane_width`), because egui on its own only
  ever remembers what a panel last rendered at — the capped width. The
  splitter is off while the cap holds: the window is the one making that
  split. The popup layout (`ColumnPlan::popup`: not even the two minimums
  fit side by side) shows the list alone, and a click on a mail opens the
  pane over it as an `Area` — above the list, under the title bar (which
  `move_to_top` keeps there; a new area is otherwise born above it) and
  under the modals — with a round, chevron-only back button floating over
  the mail's top-left corner (`POPUP_BACK_DIAMETER`), shadowed with the
  window's own shadow — the same one the `Load older mail` button floats on —
  so it reads as on top of the mail rather than as a hole in it; it is the one
  way back to the list, and closing the mail is what it does.
  The navigation is not shown at all in that layout. Re-expanding the
  navigation after any automatic collapse stays a manual choice, exactly as
  it was when the threshold was a constant.
- Layout is a three-column mail client: left navigation (collapsible via
  the title-bar toggle; auto-collapses when the window is narrower than the
  three columns need — see the bullet above), the central column (while a search is
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
  pane the way a mail on its own does. **A mail's head is its sender**: the
  list's own avatar in front of the name (`AVATAR_DIAMETER`, with the sender's
  `sender_color` and `sender_initial` — the same circle the rows draw), the
  mail's five actions at that row's right end (`mail_sheet`, the bar's cells in
  a smaller square, `CARD_ACTION_SIZE`, drawn `text_weak` until the pointer
  brings one to the text color — delete, label, forward, reply all, reply: the
  bar's run with flag and export left out, and the label one opens the label
  popup on *this* mail rather than running an action), the chips of the labels
  it carries while it has any (`label_chips`), and the
  subject, the addresses and the date under it. Each card is drawn by the pane and not
  by a frame around its mail: `mail_card` reserves the shape and the pane
  fills it in from the pane's own left edge to its right one, because a card is
  the pane's surface — the mail itself is never tinted, whatever mark it
  carries: a flag is shown in the list and on the bar's cell, not on the mail —
  and a mail wider than the pane scrolls sideways over its
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
  Select-all takes what the list shows rather than what the folder holds: a
  view filter that hides mail must not have it checked by a click on that row
  (`ListRows::shown_indices`; the label and search lists do the same over their own
  rows).
  In select mode the mail actions' Delete is the only live cell
  (`mail_action_enabled`): reply, reply all, forward, label, flag and export are
  all about the one mail the reading pane shows, which is not what the click was
  about, so they are drawn dimmed with a tooltip saying so. A checked selection
  is also the one thing that keeps the run on the bar with no mail open — the
  pane's row waits for a mail otherwise (`titlebar_anchors`). Delete takes the
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
  it sits beside the toggle rather than with the other actions over the reading
  pane; **those three are the bar's first group**, and the one thing in the bar
  that never gives way — compose is always available, so it is always there; its
  glyph is a page with a pen writing on it
  — `paint_compose_icon`, one of the bar's own 24×24 outlines like the mail
  actions' below — where the icon set's pencil used to be. The page is a
  rounded-rectangle outline whose top-right corner is left *open*, and the pen
  is the straight stroke that crosses exactly there: no stroke of the page ever
  touches it, which is what says the pen is writing on the page rather than
  lying on top of it), the search box (a field with the magnifier inside
  it, one widget,
  whose click opens the search popup), the **filter** cell (a funnel, the
  list's own view control, in the box's group — a cell's gap to its right —
  because it is about what the *column* shows, which is what the box is about
  too; the accent while any view is ticked, and a menu of them under an "All"
  row, `filter_popup`), the **sort** cell (two arrows, one up and one down, right
  after the filter and in the same group: which order the column is in, from
  `sort_popup`, accent-coloured while it is anything but the default), the group
  of mail actions
  (`titlebar_mail_actions`: reply, reply all, forward, label, flag, export,
  delete, one drawing function per glyph, each from a 24×24 outline through
  `glyph_point`), the
  icon-only **Sync now** cell, the
  preferences cog, drag area,
  double-click maximize, and the
  minimize/maximize/close buttons sent as `ViewportCommand`s.
  **The two middle groups are anchored to the columns under them**, not to the
  room left over (`titlebar_anchors`): the box starts at the message list's own
  left edge and the run at the reading pane's, so each sits over the column it
  belongs to and follows that column's splitter — and each is clamped clear of
  the first group, which is what keeps the box off the brand when the navigation
  is collapsed and the list runs to the window's own edge. The box is the one
  thing the bar gives up as a window narrows: its own width, then its floor
  (`TITLEBAR_SEARCH_FLOOR`), then nothing, with the search popup still a Ctrl+F
  away (`titlebar_search_width`). Its width is planned *first*, and the two
  cells beside it are what fits in what the box leaves of its room — the filter
  and then the sort, in that order, so a bar with room for one of them has the
  filter: a bar with room for the box but not for a cell beside it draws the box
  and neither. The box's width never depends on them, which is what keeps
  resizing a window from flickering the three against each other.
  **The pane's run waits for a mail**: it is drawn only while one is open, or a
  selection is checked (which is what Delete takes), because seven dead glyphs
  over an empty pane said nothing and cluttered the bar. It comes and goes over
  `MAIL_ACTION_FADE_TIME` rather than blinking, and it is given up — not
  squeezed — when the pane has no room for it beside the window controls: it may
  sit over the pane's own edge by up to `MAIL_ACTION_OVERHANG` (the card's
  margin) and never within `MAIL_ACTION_EDGE_GAP` of the controls, and where it
  cannot be this pane's run any more the mail's own card head carries the same
  cells.
  **Sync is not one of the first group's cells**: it is about every account
  rather than about the mail the pane shows, so it stands with the cog and the
  window controls, in a rect of its own (`TitlebarRects::sync`) immediately left
  of the cog — **and it is where the app's sync state is shown**: a spinner in
  place of the glyph while any account is syncing, a dot in the corner when one's
  last sync failed, and the accounts' own words in the tooltip. That is what the
  status strip beside it used to say, one glyph at a time. Folding it into the
  cell a sweep is asked for from is what leaves the pane's run its room at the
  default window width, and it means nothing in the bar moves or reflows when a
  sync starts. Every button in
  the bar is one hand-painted cell (`titlebar_button` + `titlebar_hover`): a
  `TITLEBAR_BUTTON_SIZE` (32 pt) square, centred in the bar's height,
  `TITLEBAR_BUTTON_GAP` (6 pt) from the next, flat at rest and filled under the
  pointer with a small rounded corner (`TITLEBAR_BUTTON_CORNER_RADIUS`, 6 pt)
  rather than the full-height square the bar's buttons used to fill. The left
  group, the mail actions and the window controls
  each set their row's `item_spacing.x` to that gap; the cells the bar places at
  exact x positions rather than in a row — the sync cell, the cog and the
  window controls — are filled through `titlebar_fill` instead. They are not `egui::Button`s for
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
  function, `titlebar_cells_width`, which places it over the reading pane and
  which the window controls are measured with — so a cell cannot come out one
  size in one group and another size in the next. A glyph is
  `MAIL_ACTION_GLYPH` (18 pt) off its 24×24 outline, which leaves it 7 pt of
  padding inside the cell. The run is deliberately not a framed group: the bar
  stays chrome plus flat icons, and the seven are held together by the clear
  chrome that sets them apart from the search box (`TITLEBAR_GROUP_GAP`) rather
  than by a pill — the same gap, not an elastic one, because the two groups are
  anchored to their columns now. The seven enable and disable
  together, on `mail_action_enabled`: in view mode that is
  `has_selected_message` — the reading pane's own lookup, so a uid the pane no
  longer shows counts as no mail — while select mode leaves Delete alone live
  (see the select-mode bullet below), the one of the seven that has something to
  do with more than one mail. A disabled cell drops its glyph to
  `weak_text_color`, stops sensing clicks and never fills. All seven are
  wired: reply, reply all and forward open the compose
  (`compose_about`, as the popup the pen opens), label opens the label popup
  (`label_popup`, anchored to the cell), export and delete hand the mail
  to a worker, and flag marks the mail the pane shows (`toggle_flag`). **Flag is
  the one cell with a state of its own**: while the mail carries it, the glyph is
  drawn in the scheme's `warn` — the colour the mail's row and card are tinted
  with — and the hover text says what the click would do now, "Unflag…" rather
  than what a flag is.
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
  its own content (`content`: everything left of the sync cell) and the fixed
  right end — the sync cell, a gap, the cog, a gap, the three window controls,
  ending `TITLEBAR_EDGE_MARGIN` short of the window's edge, the count coming
  from `titlebar`'s own button list so the two cannot drift — while the two
  middle groups are placed from the columns below them (`titlebar_anchors`) —
  the box and the filter cell as one group, the pane's run as another — which is
  what makes the bar read as a map of what is under it rather than as a row of
  things that happen to be in it. Those two rects are a cell's own band rather than the
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
  The box is the one thing given up, and in one order (`titlebar_search_width`,
  pinned by `the_title_bar_gives_its_room_up_in_one_order`): its own width
  while there is room, its floor while the room is short, and nothing below that,
  with the search popup still a Ctrl+F away. Every width is monotone in the room,
  so resizing a window never flickers between two shapes. The pane's run has its
  own rule instead — it is not the room's leftovers, it is the pane's row: it is
  placed at the pane's edge (or over it by no more than `MAIL_ACTION_OVERHANG`),
  and given up when the window controls leave no room for it there, which the
  mail's card head answers for. The plan and the widget also
  have to agree on what "the box's width" means: `TextEdit::desired_width` is the
  *whole* box in 0.36, the margins coming out of it rather than being added to
  it, which is why the rect the widget hands back — the frame, not the inner text
  rect — is used as the box as it is. Re-adding those margins, as the 0.31-era
  code did, puts the magnifier 22 pt left of the box, over the compose cell, and
  stretches the click target over it. The test named
  `the_search_box_is_as_wide_as_it_was_planned` is what keeps the plan's number
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
  it, and the one action that asks where. **The worker asks the desktop first**
  (`crate::portal::save_file`, the XDG portal's `SaveFile` — the same
  hand-written D-Bus conversation the attach card's chooser uses, on a worker
  because it does not answer until the user has), then fetches the message's
  original RFC 5322 source from the server — IMAP
  `UID FETCH <uid> (UID FLAGS BODY.PEEK[])` through `ImapClient::uid_body` (PEEK
  on purpose: fetching a mail to export it must not mark it read), POP3 `UIDL` +
  `RETR` through `Pop3Client::retr_by_uidl` — and writes it byte for byte where
  the dialog said. The order is the point three times over: a cancelled dialog
  costs no round trip and says nothing (closing a save is not a failure), and the
  account's spinner — which says the *server* is being asked — does not turn while
  the user is choosing a name. The job's `dir` is both the folder the dialog is
  suggested to open on and the destination when there is no dialog to ask
  (`storage::downloads_dir`, made on the first export): a session with no portal
  is not an export that cannot happen, and the fallback hides nothing, because the
  notice names the file it wrote.
  **The dialog's answer is the file's name**, extension and all: the dialog was
  drawn with the name in it, and appending a suffix to what the user left would be
  Kimo deciding something the dialog already asked. That file is written by
  `write_at` — through a hidden `.kimo<pid>_<n>.part` beside it and a rename
  into place, so that replacing a file (which the dialog has already asked
  about) leaves either the file that was there or the whole new one, and never
  half of either. The no-dialog writer is `write_file`, the download's too:
  `<stem>.eml`, numbered `<stem>-2.eml` and on when a name is taken, and
  *created* (`create_new`) rather than merely asked about, so two exports of the
  same mail cannot overwrite each other. The name is made from the subject, with
  what a file name cannot carry replaced and the whole capped at
  `EXPORT_STEM_BYTES`
  (`export_stem`; `slug` would ASCII-fy every non-Latin subject away) — offered to
  the dialog as well as used by the fallback.
  **The original is the point.** What is stored here is the parsed mail, with
  each attachment's name, type and size but never its bytes, so a `.eml`
  rebuilt from the database would not be the message the sender wrote, while
  this one is the message whole, attachments and all. The price is that mail the server no longer has cannot be exported at
  all, and says so rather than writing something close (`Event::SyncFailed`,
  prefixed "export:" — the local copy is not a substitute for the original).
  The job brackets its round trip with `SyncStarted`/`SyncDone` like a delete,
  and answers `Event::MessageExported { path }` — with the path, because the name
  may be the dialog's or the fallback's and only the worker knows which it got.
  The app raises a **desktop notification** for it (`exported_notification`,
  "Mail exported" / "Saved to <path>") rather than the timed line above the
  message list that a *downloaded attachment* and a finished send use
  (`list_banner`, the place the storage warning stands): the export's click was
  in the title bar, and the file it wrote is not in the list. Nothing is raised
  when the dialog was closed — nothing happened, so there is nothing to say.
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
  `storage::downloads_dir` — the directory an export with no dialog to ask
  writes into, the same one-line `Saved to <path>` notice and the same
  `create_new` numbering, through the writer they share (`write_file`; the
  download's own rules are `download_name`, which keeps the sender's file name,
  its last path component and a plausible extension, and `safe_stem` behind
  both).
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
- The compose card comes in two containers (`ComposeSpot`) and is one card
  (`compose_card`). The title bar's pen — and its reply, reply all and
  forward — bring it up as the `egui::Modal` it always was (`compose_window`):
  a card `COMPOSE_WIDTH` wide in the foreground layer, whose backdrop dims the
  columns and the bar and takes their clicks, so a mail is written with the
  rest of the app out of the way. The reply, reply all, forward and delete
  cells on a mail's own header (`mail_sheet` — the title bar's own cells and
  glyphs, at the right end of the card's head row, delete outermost) open the same
  three *on the spot*: one more card of the reading pane, right under the
  mail answered (`compose_inline_card` — the pane's own surface at the pane's
  width, a card gap above it), and the mail stays on screen above its answer
  until the card is done — sent or discarded, or, in the popup layout, backed
  out of by the floating back button, which closes the card with the view it
  was opened in (a draft whose Send is in flight is owed its event, and the
  button waits for it). The card answers the mail whose header was clicked —
  in a conversation that need not be the mail the pane was opened on — and
  `compose_anchor` holds which mail that is; a draft whose mail has left the
  pane (another folder opened, the mail deleted) moves to the popup window
  rather than sitting under mail it was never about (`compose_anchor_on_pane`,
  from `ui`). There is one draft at a time — `KimoApp::compose` holds it —
  and the pen leaves an open card alone rather than resetting what has been
  typed into it; the three answers do not (the click is the user's asking for
  another draft), except a draft already on its way out, which no click
  replaces: its Send is owed an event.
  **The three that answer a mail are made of it** (`ComposeDraft::reply`,
  `reply_all`, `forward`): a reply is addressed to the sender alone, a reply
  all to the sender and every other recipient — the account's own address
  excluded, so nobody is sent their own mail — and a forward to nobody until
  the sender chooses. All three quote the original under the new text
  (`quoted_body`: who wrote it and when, their text a `>` line at a time —
  the text part, or the HTML rendered down to words, because the card is
  plain text), and prefix the subject (`answer_subject`) unless it already
  carries a prefix, whatever prefix the sender's client wrote
  (`threads::strip_subject_prefix`'s table, the threading's own).
  From is the configured accounts, defaulting to the one whose mail is open and
  falling back to the first (`compose_from`); To, Cc, Bcc, Subject and the body
  are plain strings in `ComposeDraft`, deliberately not widget state: what a
  card opens with is the draft's own — nothing, for a new mail; the mail it
  answers, for the three. **The on-the-spot card is the shorter form**
  (`compose_card`’s `inline`, which is what `ComposeSpot::Pane` means): an
  answer written under its own mail shows From, To, the body, the files and the
  two buttons, and nothing else — in that order, a letter’s: the mail is what
  the card is for, and a file is something put *with* it (`compose_body`
  before `compose_files_row`) — while the popup keeps its form’s order, files
  over body, where the attachments belong to the fields above them. It has no
  heading — the mail above it says what it answers — but it has a **head bar**:
  the avatar of the account the answer goes from, drawn in the label column’s
  own slot, and the From list
  beside it, with the separator under the row that the popup’s heading has
  above its form. The list is *flat* there: `flatten_widgets` takes the combo’s
  outline and fill off in that Ui and leaves the hover, because an outline
  belongs to a form of boxes and this line is the reply’s own from. **The flat
  list ends with the To row’s chevron** rather than the filled triangle a combo
  box carries (`paint_from_chevron`: the To row’s own drawing, painted in the
  place egui would put its triangle, down while the list is away and up while it
  is open — the head of a letter ends the way the row under it does, weak until
  the pointer finds it). The popup’s own From keeps its triangle, a box among
  boxes. **To is
  flat beside it**, for the same reason: the row’s field is added with
  `Field::flat`, a field whose frame draws nothing — so it has no box focused
  or not — which is also what a flat *field* wants where `take_box_off` would
  do: the visual it clears is the colour of the text selected inside the
  field, and a flat field has no business recolouring that. The popup keeps
  its To box, as it keeps its Cc, Bcc and Subject. The face is the one a mail’s
  card wears (`sender_color` and `sender_initial` of the
  account’s address and name, at `AVATAR_DIAMETER`), so the card says who it is
  from the way every other card in the app says who it is from. **To ends with
  the fold chevron** (`paint_fold_chevron`, in a cell a field tall): the Cc
  and Bcc lines are the popup’s rows, but on the spot they are one click away
  under To (`compose_to_row`) — flat like the lines above them, pointing down
  while they are away and up once they are under the row — and a card opens
  with them away (`ComposeDraft::copies_open`), so an answer that needs no
  copies shows the one address it goes to. A Reply all’s own Cc is still *in
  the draft*, and read — and edited — as soon as the chevron is clicked.
  Subject stays the popup’s row, and stays in the draft with it. The popup
  keeps its heading and all five rows, and one draft moves between the two
  containers with only the container changing.
  The footer is the one other thing the two do not share: the on-the-spot card
  draws `compose_button`’s pair — an icon and a word in one tall target, Send
  in the accent because sending is what the card is for (`paint_send_icon`, the
  plane) and Discard an outline beside it that turns the scheme’s danger on
  hover (`paint_delete_icon`, the trash, the treatment every delete in the app
  gets) — while the popup keeps egui’s own two, a line of its form. **The
  card’s foot has no rule over it** either: the card is a letter, and the mail
  above it — not a line — is what its end is read against. What it has to say
  (the server’s refusal, that the mail is on its way, why Send cannot be
  pressed) stands on a row of its own over the buttons, and the pair opens the
  row at the card’s left edge — the side the mail above is read from — rather
  than ending it at the right, where the popup’s form keeps its own: Discard
  first and Send beside it. That row is a *bounded* one (`ui.horizontal`, the
  height of what is in it): a `with_layout` over the space left would center
  the pair in all of it and carry the card’s surface down to the pane’s floor,
  which is what left a band of nothing between the reason and the buttons —
  **the card ends where its content does**, `CONTENT_PADDING` under the last
  row on it. The popup’s reason reads beside its two, over the form’s own
  separator. The boxes are a fixed label column plus what `COMPOSE_WIDTH`
  leaves; the body is `COMPOSE_BODY_HEIGHT` tall and scrolls inside that
  rather than growing the card — and **the box fills that height and, on the
  spot, the card’s whole column** (`compose_body`’s `min_size` and the width
  `inline` hands it: a letter’s text block, padding to padding, where the
  popup keeps the form’s field column), not the dozen lines an empty draft
  needs — so the row under it is never read across a band of the card’s own
  surface and the mail being written is as wide as the card will hold. **The
  box is the card’s, not the text’s** (`compose_body`): a text edit draws its
  own frame about what it holds, and an answer holds a quoted mail — more
  text than the `COMPOSE_BODY_HEIGHT` viewport holds — so a frame of the
  field’s own would run past the room at both ends and go where the scroll
  goes: its top out of sight after a scroll down, its bottom out of sight
  before one, which reads as a box with two sides. The fill and the line are
  painted here instead, in the viewport’s own rect, the line answering the
  keyboard the way every field’s frame does (`Visuals::selection`), and the
  field is given no frame of its own (`take_box_off`). **The
  area is claimed outright** (`compose_body`’s `scope_builder` over a rect of
  its own), because a window too short for the card is the pane’s to scroll:
  a scroll area left to the room under the To row is squeezed to a few lines
  there, which cuts the box off and leaves the card’s surface not even
  covering what is left of it. The
  first frame puts the keyboard where the card’s own next move is: To on the
  popup’s form — and on a forward opened on the spot, which has no recipient
  yet — and the body on an answer that already knows who it goes to, with
  `cursor_at_end(false)` so the caret starts the reply *above* the
  quoted original rather than after it. That same first frame is what brings an
  on-the-spot card into view: `compose_inline_card` reads the draft’s `focus`
  before `compose_card` takes it and scrolls the pane by no more than the card
  needs (`Ui::scroll_to_rect(.., None)` — the shortest scroll that shows it,
  and its head when it cannot all fit), because a reply cell at the top of a
  long mail would otherwise open its card below the fold with nothing on
  screen to say the click had done anything.
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
- **The automatic sync is a preference, and it is the only switch**: the
  preference `auto_sync` (off unless asked for) sweeps *every* account every
  `auto_sync_minutes` (15 by default, clamped to 1–1440) — `KimoApp::sync_all`,
  the same sweep the title-bar Sync cell runs, so an account that is not
  configured is not asked and a failed one reports itself like any other sync.
  What it adds to the startup sweep and the button is the clock and nothing
  else, and the preferences page's controls are live whether or not it is on.
  `pump_auto_sync` is the
  clock, called once a frame: `auto_sync_wait` is the whole decision (nothing
  while off, otherwise the interval left since the last sweep, never negative),
  and the wait it returns is what the app asks egui to repaint after — no other
  timer runs, and a frame with nothing to draw would otherwise never come. A
  sweep already in flight is left to finish and the clock is set anyway, so a
  busy account is asked again a full interval later instead of every frame; the
  clock is reset by `sync_all` itself, so pressing Sync is not a reason to be
  swept a moment later. Changing the preference restarts the clock, and the
  preference is what `settings.json` keeps — the timer is not persisted and
  starts fresh with the process. **A sweep shows itself in the sync cell**, not
  in a strip of its own: a spinner takes the glyph's place while
  `sync_in_flight` holds, a dot sits in the cell's corner while an account's last
  sync stands failed, and the accounts' own words — "syncing…", the error — are
  in the tooltip. What the bar shows of a sync is therefore in the control that
  starts one, and nothing else in the bar has to move aside for it.
- `preferences_modal()` is the one place to configure Kimo: an
  `egui::Modal` (backdrop click or Escape closes it) with a sidebar of
  pages — **General** (the mail list's threading, the automatic sync and the
  notifications), **Accounts**
  (account list with
  use/edit/remove, and the add/edit account form in place of the list: it is
  pre-filled from the account it was opened on, and `Save & sync` updates that
  account and syncs it) and **Appearance** (theme, accent, Simple/Blitz HTML
  renderer, the interface/fixed-width fonts — all persisted in
  `settings.json` — plus a preview of the palette the theme and the accent
  resolve to). Configuration belongs on a page there, not as another
  title-bar toggle; the title bar keeps only actions (syncing, window
  controls), and switching accounts is a navigation-column affair.
- Colors live in one place, `src/theme.rs`. `Theme` (system/light/dark — the
  only part that is persisted) resolves to a `ColorScheme`, and both defaults
  are filled in slot by slot, `ColorScheme::light()` and
  `ColorScheme::dark()`: surfaces (panel — the chrome; content — the message
  list and the reading pane's card; window, card, hover, input, code, border;
  `flag`, the warning tint a flagged mail's row in the list takes;
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
  `theme::hex`), and the one slot of it a reader can change is the accent:
  `theme::Accent::ALL` is the six choices, the page draws them as chips
  (`accent_chip`, the one in force ringed), `settings.json` stores the
  key, and `ColorScheme::with_accent` puts the pair into the scheme the
  theme resolves to — `Accent::KIMO` is the pair the schemes already hold,
  so the default changes nothing. An accent carries the colour *and* the
  tint selected rows are filled with, once per theme, because a violet that
  reads on near-white paper is not the violet that reads on a dark surface.
  Everything else about the palette is still not editable, which is why
  nothing else about it is written back to `settings.json`. `theme.rs`'s tests pin the
  contrast floors **over every accent of `Accent::ALL`, in both themes** (body
  text ≥ 4.5:1 on its panel *and* on its content, accent
  ≥ 3:1, text on the selection, hover and flag fills ≥ 4.5:1, weak text on a
  selected row ≥ 4:1, the close glyph on its
  red ≥ 4.5:1, both schemes' surfaces on opposite sides of the brightness
  scale, and the mail rows' edges fainter against the list than a hairline,
  with the top edge fainter than the sides); keep them green when you touch a
  color.
  **A focused field's box is the accent** (`Field`, app.rs). egui draws a text
  edit's frame from `Visuals::selection.stroke` while the keyboard is in it —
  the same slot as the colour of *selected text* — so a field that took that
  line for its outline would recolour every selection in the app. Instead the
  field is added with a frame that draws nothing (`field_frame`, whose margin is
  the text's own) and Kimo paints the box itself: the theme's input fill under
  the text, and over it the border — or the accent, with the keyboard in the
  field. The writing area is the same box drawn by `compose_body`, around the
  room the card gives it rather than around the field's own height. The title
  bar's search box is the one field not drawn this way: it is a non-interactive
  look-alike behind the search popup, with the glass fill and radius of its own.
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
  **The labels are the column's last group, under every account**, which is
  where a thing that belongs to no account can be: `label_group_header` is
  the word alone — no disclosure triangle, because there is no section to
  collapse, and no count, because the labels are not one list — at the left
  edge the accounts' names start from, and the rows are indented under it the
  way folders are. A row (`label_row`) is a folder row's shape with the
  bar's tag glyph where the icon goes, the name, and the amount of mail carrying
  it right-aligned; the whole row is the click target, and one whose label is in
  the filter takes the selection fill. A label no mail carries any more is drawn
  weak and stays in the column: it is the user's own word rather than a count of
  the mail under it.
- **A label is the user's own mark on a mail, and a filter of its own.** The
  two are one feature: what makes a label first-class is that the column can
  look at the mail that carries it, and what makes the column able to is that
  the mark belongs to the mail. A mail lives in exactly one folder — where the
  server put it — and carries any number of labels; the name is the whole of
  one, unique case-insensitively, so `Work` and `work` are the same label
  rather than two rows (the popup keeps the spelling it was made with).
  `MailMessage::labels` is the app's copy of them, filled by
  `db::load_mailboxes` from the join table and **never by a fetch**: nothing
  about a label is asked of or told to a server, which is why a re-synced mail
  keeps the marks it had here (the merge in `Event::Messages` takes the
  fetched mail's fields and the held labels).
  **A label is made by being used.** There is no "new label" form: the popup's
  box names one, and `db::set_labels` creates whatever row it needs as it puts
  it on the mail — one transaction, and the message's own rows replaced, so what
  a mail carries afterwards is the set that was asked for rather than something
  merged with what was there. The popup (`KimoApp::label_popup`) is the label
  cell's menu — `egui::Popup::menu` with `CloseOnClickOutside`, so ticking
  several labels is one visit — anchored to the cell that opened it, opened from
  the title bar's cell for the mail the reading pane shows and from the cell on
  a mail's own head for that mail (`mail_sheet`; the head's five cells are the
  bar's run with flag and export left out). Its checklist is every label, ticked
  for the ones the mail carries, and a tick or an untick writes the whole set
  through `Job::SetLabels` — the app's copy first, so the next frame draws it,
  and a worker to make it durable. Its box is emptied each time a popup opens:
  what was typed for one mail is not a draft for the next.
  **The mail says what it is labelled with**: a card draws its own labels as one
  run of chips under the head (`label_chips`), one per label and only while it
  has any, so an unlabelled mail's head is still the first thing on it.
- **A flag is the reader's own mark too, and the one mark the server keeps
  as well.** The two halves are what make it different from a label: it is one
  state per mail rather than a set (`MailMessage::flagged`, schema v9), and it is
  `\Flagged` on an IMAP server, so flagging a mail here flags it everywhere.
  The title bar's flag cell runs `KimoApp::toggle_flag`: the app's own copy of the
  mail is flipped first, so the warning fill appears under the pointer rather
  than a round trip later, and `Job::SetFlag` writes the database and pushes `UID
  STORE +FLAGS.SILENT (\Flagged)` — or `-FLAGS` to take it off — for an account
  that speaks IMAP. The local write happens first, like `mark_seen`'s: a server
  that refuses the `STORE` does not cost the reader the flag they just set, and
  the failure is reported on the sync cell like every other account failure. A
  POP3 account has nowhere to push it and keeps the flag in the database alone.
  **A fetch is where the flag can also arrive**: `MESSAGE_UPSERT`'s `INSERT` carries
  `flagged`, so mail the server reports as flagged is stored with the mark —
  and the column is in neither the `SET` nor the `WHERE`, so a mail the database
  already has keeps what the reader did with it. That is what makes a POP3
  re-fetch — which rewrites every message it holds, and has no flags to report —
  harmless.
  **A flagged mail is shown in the list, and only there.** Its row takes the
  scheme's `flag` tint and draws the flag glyph at the end of its title line
  (`message_card`, beside the paperclip for a mail that carries both), while the
  reading pane is untouched: the mail itself keeps its plain `content` card, so
  what a flag changes is where the mail is *listed*, not how the mail reads. The
  selection and the pointer are stronger than the mark and are painted over the
  row's fill — the fill is selected, else hovered, else flagged — which is what
  the glyph is for: it is the one thing on the row that says *flagged* whatever
  else the row is doing, and the bar's own cell says it in the same colour. A
  conversation's row stands for its mails and carries the mark if any of them
  has it (`Thread::flagged`), and a search hit carries it too
  (`SearchHit::flagged`, off the query rather than out of the search index), so no
  re-index is needed to see it.
- **The label filter is an AND, and it is the mailbox's rather than a
  folder's.** A click on a label row adds it to `KimoApp::label_filter`
  (`toggle_label`), a click on a selected one takes it out, and the message
  list becomes every mail that carries *all* of them — across folders and
  accounts, which is what "first-class filter" means here and why those rows are
  drawn like search hits (`labelled_row`: the third line is where the mail
  lives) and are flat rather than threaded. The list is derived rather than
  scanned per frame (`LabelView`, `refresh_label_view`, keyed by
  `labels_rev`, the accounts' `folders_rev` and the filter), the way the
  list's conversations are. Nothing about the filter is persisted: which labels
  you are looking at is a per-session thing, like select mode and the open
  conversations. A query and a label filter are two answers to what the middle
  column shows, so a label click ends an active search (`end_search`) — while
  a folder click, and opening a mail from the filtered list, deliberately leave
  the filter alone: the pane's folder is where the mail it shows is filed, not
  what the list is showing.
- **The list filter is a set of optional views, and they are an AND.** The title
  bar's filter cell opens a menu over whatever the column lists —
  `ListFilter::Unread`, `Flagged`, `ToMe`, `HasFiles`, each an independent row
  (ticking one never takes another off, and the menu stays open across ticks) —
  and a mail is listed only while it passes **every** ticked view
  (`ListFilters::matches`, an `all` over the set). A row is drawn by the app rather
  than by a stock checkbox (`filter_menu_row`): **the view's own mark at the left,
  its name, and the tick at the row's *end***, where a menu's check belongs — a
  checkbox puts its box in front of the name, which is where the mark goes. The
  marks are the list's own where the list has one (the flag, the paperclip),
  and an envelope, an envelope with the new-mail pip, and a person for the
  rest, all in the 24×24 outline style the whole icon set is drawn in. No tick at all is the menu's
  own **All** row: it is drawn ticked exactly while the set is empty and ticking
  it clears the rest, so "all mail" is the absence of views rather than one of
  them. The cell is accent-coloured while any view is ticked and its tooltip
  names them ("showing unread + flagged mail"). It applies
  to all three lists the column can show: a folder's mail (filtered before the
  conversations are built, so a conversation's own row speaks for the mails
  that are left — `thread_of_shown` — and the reading pane shows those same
  mails, since `open_conversation` reads the derived threads), a label filter's
  matches and a search's hits. The search popup's own "Only" boxes are the other
  layer: those narrow the *query*, in the database, and a query filter and a
  view filter together are an AND.
  **The predicates are one function over four facts** (`ListFilter::matches` over
  `FilterFacts`), because all three rows can answer them: a folder row from
  `MailMessage`, a hit from `SearchHit`, a label row from `LabelledMail` — which
  is why both of the latter carry the `To` header. "To me" is a substring test
  against the accounts' own addresses (`KimoApp::my_addresses`: each account's
  username *when it is an email address*, the rule the compose applies to what a
  mail is sent as), so an account whose username is a bare login leaves the
  question unanswerable and the menu row says so rather than filtering to
  nothing. **The derived rows carry the set they were built with**
  (`ListRows::filters`, beside `threaded` and the revisions), so a change derives
  them again and a frame that changed nothing derives nothing; the hits and the
  label matches are filtered as they are drawn, since neither is cached for the
  frame. A view that hides every mail leaves the column saying which kind of
  empty it is — "No mail matches this filter." against the folder's own "This
  folder is empty." and a search's "No hit matches this filter." — and
  **select-all takes what the list shows** (`ListRows::shown_indices`), never
  mail the filter has hidden.
- **The list sort is two independent choices, in one menu.** The title bar's
  sort cell (`sort_popup`) holds the *order* — "Newest on top" (the default, the
  order the folder arrives in) and "Oldest on top" — and the *key* — date,
  sender, subject — as two groups under their own small headings, each a choice
  of one drawn as a row with the tick at its end (`menu_row`, the same row the
  filter menu uses). The two are one comparison
  (`ListSort::compare`): the key decides, the order group turns it round, and
  where the mail sits is the tie-break — which is what keeps two mails equal by
  the key in the folder's order instead of swapping between frames, and what
  makes the default a no-op on the list as it already is. The keys are what a
  row *shows* (`SortKey`): a conversation's row is sorted by its newest mail's
  date and sender and by its own subject, and names are compared
  case-insensitively the way a reader would.
  **It is a view of the column like the filter**, and the two share a cell's
  worth of state: the derived rows carry both (`ListRows::filters` and
  `ListRows::sort`), so a change derives them again, the flat list is drawn in the
  order `flat_order` worked out rather than reordering the folder's own slice,
  and the label view's matches are sorted in their derivation (whose key carries
  the sort too) with its own total order behind them, since a label spans every
  folder and account at once. **A search's hits keep the query's order**: they
  are ranked, and relevance is what a search is for — the sort is about the
  lists that are the mailbox itself.
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
  `mail.png` next to the input. `dump_ui` is the same idea for the app's
  own frames and needs no compositor at all:
  `cargo test -- --ignored --nocapture dump_ui` drives the clicks
  itself and rasterizes the frames — the columns, the card, the label popup,
  Preferences → General — straight from egui's own triangles into
  `target/ui-*.png`
  (`KIMO_UI_DUMP` picks another directory), which is how a layout is looked at
  on a machine whose display is somebody else's. Its frames have to go through
  `collect_frame` or a glyph the frame was the first to draw comes out blank:
  egui sends the font atlas whole once and patches after that. Looking at a
  picture is not running the app — click through the real one before calling a
  change done. For cost rather than fidelity, use
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
  afterwards either — the file lands in `~/Downloads` (a download does not ask
  where, unlike Export) and the desktop's own programs are what open it. Export
  remains the way to get the whole message, attachments included, as one `.eml`.
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
- Labels are **local to this machine**: nothing about them is asked of or told
  to a server, so another client's labels are invisible here and Kimo's are in
  `mail.db` alone — a label names a message row, so mail that comes back under
  a new UIDVALIDITY generation comes back unlabelled. A label cannot be renamed
  or deleted (the popup only puts one on a mail or takes it off, and a name no
  mail carries any more stays in the column), nothing is labelled in bulk
  (select mode leaves Delete as its one live action, and the label cell on a
  mail's head is about that mail), and the filter is in memory only.
- **A mail can carry files, and the card is where they are chosen.** Two ways
  in, and one area that says both (`attach_card`): **a click opens the desktop's
  own dialog** (`crate::portal`: the XDG portal's
  `org.freedesktop.portal.FileChooser.OpenFile` over D-Bus, which runs the dialog
  in the *compositor's* process — nothing is linked in, and it is the one way
  that works on Wayland at all), and **a file dragged from a file manager** lands
  on the window (egui's `dropped_files`, read in `KimoApp::ui` rather than in the
  card: a drop belongs to the frame, and one that arrives with no card open
  *starts* a card with the file on it, which is what dropping a file on a mail
  client means). The area replaces the button, path box and Attach button that
  used to say the same thing — the dialog is the way in, and a typed path was
  only ever the fallback for a session with no dialog to open. What a file that
  cannot be attached has to say — a directory, something that has gone, a desktop
  with no portal — is drawn under the area, never beside it.
  ```
  Files  [ file card  ✕ ]  [ file card  ✕ ]
         [ Click to select files, or drag files here ]
  ```
  **A file is a card**, the reading pane's own (`attachment_card`, drawn here
  with a click that takes the file off the mail rather than fetching it, which is
  the one thing the tooltip's last line varies by), wrapped across the field
  column; the same file twice is two parts, because a mail may carry one file
  under two names. Each of those cards carries a
  **small round cross** in its right-hand gutter — the room for it is part of the
  card’s own width, so a long name is cut before it, never under it — and the
  cross is a widget of its own: the treatment the row delete button gets (muted
  until the pointer is on the button, then the scheme’s `danger_hover` fill with
  the glyph in `on_danger`), the tooltip "Take this file off the mail", and a hit
  box of its own beside the card’s body, which does the same thing and says so.
  The reading pane’s cards have **no** cross (`removable: false`): a mail’s own
  attachment is not Kimo’s to take away. On a Wayland session the drag half of the area’s sentence is a
  promise the platform does not keep: winit 0.30 has no drag-and-drop in its
  Wayland backend, so no `HoveredFile` or `DroppedFile` ever reaches the window
  and the card's "Drop the files here" state cannot appear. The dialog is
  therefore the way in that has to work.
  **The draft holds paths, not bytes** (`outgoing::OutgoingAttachment`: the path,
  the name, and the `Content-Type` its extension suggests, `content_type_of`), and
  the worker reads them as it sends (`load_attachments`): the UI thread reads no
  files at all (rule 3), and what goes out is the file as it is when the mail
  leaves rather than as it was when it was chosen. A file that cannot be read
  stops the send with its path in the reason — a mail that goes out without the
  file its writer attached is worse than one that does not go out — and the same
  failure is what the SMTP send and the Sent copy both see, because the message
  is rendered once (`send_message`) and the same bytes are delivered and filed.
- **A mail with files on it is `multipart/mixed`.** `outgoing::render` writes the
  headers it always did and then, instead of a bare `text/plain` body, a boundary
  and one part per thing: the letter first — so a client that shows only the
  first part of a multipart body still shows the letter — then a part per file,
  each with its `Content-Type`, `Content-Transfer-Encoding: base64` and a
  `Content-Disposition` that names it. The boundary carries an underscore
  (`=_kimo_<id>_=_`), and the base64 alphabet has none, so no encoded part can
  contain it — which is the one thing a boundary has to be able to say. A name
  that is not ASCII is written twice: a quoted form with the name reduced, which
  is what an old client shows, and RFC 2231's `filename*=utf-8''…`, which is what
  a client that knows about it reads. The files are read whole into memory —
  there is no streaming and no size cap — which is the price of the rest of the
  app being the same.
- A written mail goes out over SMTP and no further: no HTML body (the card is a
  text box, so the letter is `text/plain; charset=utf-8`, base64-encoded), no
  drafts — closing the card is the end of
  the mail, and a failed send keeps the draft in the card but nowhere else — no
  outbox, queue or retry, no DKIM, and no OAuth2: submission authenticates with
  `AUTH PLAIN`/`AUTH LOGIN` over TLS, or not at all when the server asks for no
  login. Every title-bar mail action is wired (reply, reply all and forward open
  the compose, label opens the label popup, flag marks the mail, export and
  delete hand it to a worker, and the compose pen is wired; the mail's own header
  carries the same cells minus flag and export and answers on the spot — see
  those bullets above).
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
  **A mail's colours are adapted to the theme** (`html::adapt_color`), which is
  what makes this renderer theme-following rather than theme-blind: mail is
  written for white paper, and the dark theme draws it on a surface that is
  nothing like it — the sender's `#000000` on the card is 1.4:1, which is not
  text. A colour a declaration sets is read as CSS and moved through OKLCH, so
  what changes is lightness and not identity: the **light** theme draws every
  colour as written; in the **dark** theme a background above 0.85 lightness
  becomes the surface mail sits on (`0.12, 0.12, 0.14`), text below 0.25
  becomes (`0.90, 0.90, 0.92`), text above 0.85 is left alone, and the middle
  keeps its hue and chroma with its lightness flipped. It happens in
  `apply_decls`, once per declaration, so what a wrapper sets is inherited down
  the tree already adapted, and the alpha a colour carries is kept.
  **The flip is unconditional**, so a mid-tone above half lightness comes back
  *darker* than it went in: `color: red` (`#FF0000`, l 0.63) is drawn
  `#9F0000`, which on the dark card is less readable than the red was — lifting
  instead of flipping is a one-line change if the owner wants it. **Blitz
  transforms nothing at all** — it draws the sender's page, on a paper of its own
  (see the next bullet but one).
- Blitz renders tables in the *separate* borders model: `blitz_render`'s
  `COMPAT_CSS` overrides `border-collapse` because blitz's collapsing paints
  the first cell's border width/colour across the whole table — a 3px dark
  frame around every `border-collapse: collapse` table whose first cell
  declares no border, and wrong-coloured cell borders. Consequence: adjacent
  cell borders do not merge (a 1px border on both sides of a shared edge draws
  2px), and `cellspacing` stays 0 unless the mail sets `border-spacing` in
  CSS. Fixing this properly means fixing blitz, not this workaround.
- **A Blitz page is drawn on a paper** (`draw_blitz_page`, the pane's own
  doing — nothing in the render path is touched): the mail's colours are never
  transformed, but a mail that declares no background of its own would otherwise
  be composited straight onto the pane's card, and in the dark theme that is the
  sender's `#000000` on `#24272E`, 1.4:1. So the pane fills the page box with a
  paper before it blits the raster: **white** by default, and switchable per mail
  from the contrast cell at the inner end of the mail head's action run (the
  sixth cell, `header_action_center(5)`; `Paper`, `KimoApp::blitz_paper`,
  `toggle_blitz_paper`). White is the default and is *not* stored, so the map
  holds exactly the mails the reader changed, and only for the session — nothing
  goes to `settings.json`. A mail that **does** declare a background keeps it:
  `blitz-paint`'s `paint_scene` resolves the root element's background, else
  `body`'s, and fills the whole page with it, so the raster is opaque across the
  page and the paper behind it never shows. That is also why the cell is not
  hidden for such a mail — nothing in Kimo reads the mail's CSS to decide.
  `BLITZ_PAPER_DARK` is the dark scheme's own `content` surface, and the paper
  is deliberately not part of `blitz_key`: flipping one repaints a frame and
  starts no render. The cell is drawn only where a Blitz page is (`html_renderer`
  is Blitz and the mail has an HTML body), so Simple mode and plain-text mail
  have none.
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
  `\Seen` **and** `\Flagged` are taken for a message as it is fetched — the two
  states `fetched_state` reads off the `FETCH`, and the flag is kept for a row the
  database is storing for the first time — and from then on the local state is
  the only truth: `seen` only ever grows, and `flagged` is not named by
  `MESSAGE_UPSERT`'s update at all, so a re-fetch leaves the reader's own click
  where it was. Sync used to refresh the newest batch for free by refetching it;
  now that a warm sync fetches nothing it already has, mail read — or flagged —
  in another client keeps the mark it had here.
- No IDLE/push for IMAP; sync is manual or on-startup. A desktop
  notification is therefore only ever raised by a sync that ran, and never for
  mail that arrived while the app was closed until the next one asks for it:
  `Job::Sync` at startup, the title-bar button, the automatic sync, or the
  first click on a folder. A notification does nothing when it is clicked —
  the default action would mean listening for the daemon's `ActionInvoked`
  signal, which this client does not do — and its icon is scaled for the
  monitor the window is on, so a notification a multi-monitor daemon draws on
  another screen keeps that scale.
