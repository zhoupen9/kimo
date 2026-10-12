# AGENTS.md

Working agreements for AI agents (and humans) contributing to **Kimo**.
Read this before changing anything.

## What this project is

Kimo is a desktop **email client** GUI written in pure Rust:

- **GUI:** eframe/egui (immediate mode, pinned to the 0.31 line for now).
- **Protocols:** IMAP4rev1 and POP3 **only**, implemented by hand in
  `src/protocol/` — no `imap`/`async-imap`/`pop` crates.
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
- **Receive-only by design.** There is no SMTP and no mail composing/sending.
- **Tray icon:** `src/tray.rs` implements StatusNotifierItem by speaking the
  D-Bus wire protocol directly on a `std::thread` — deliberately not zbus
  (async executor, rule 2) or the GTK tray crates (C, rule 1). The icon is
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
  stay `a(iiay)` — hosts cannot read a bare byte array. The bus is silent
  for hours at a time, so reads go through `read_full`: an idle read-timeout
  expiry must never end the thread (`read_exact` turned it into a fatal
  `WouldBlock`, and the icon vanished 30s after startup).
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
3. **Never block the UI thread.** `eframe::App::update` only drains the
   event channel and draws. Any socket I/O, file scan or other potentially
   slow work must be a `sync::Job` on a worker thread. Workers call
   `ctx.request_repaint()` when they emit.
4. **No SMTP.** Requests to add outgoing mail are out of scope unless the
   project owner explicitly reopens that decision.
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
src/main.rs             entry point; installs the rustls provider (ring + FFDHE), launches eframe
src/app.rs              all UI: KimoApp state, panels, preferences modal, event handling
src/models.rs           Account / Protocol / TlsMode / MailMessage (plain types: nothing is serialised any more)
src/error.rs            MailError + MailResult
src/fonts.rs            installed-font discovery (ttf-parser scan) + egui font installation
src/storage.rs          XDG paths, settings.json, remote-resource cache, Mailbox/Folder (in memory)
src/db.rs               the mail database: schema + migrations, accounts, folders, messages, the old-JSON import
src/search.rs           mail search: the CJK bigram index, the FTS5 query builder and the filters they run against
src/theme.rs            theme choice + the light/dark color schemes and their egui Visuals
src/mail.rs             raw RFC 5322 bytes → MailMessage (mail-parser; HTML kept for rendering)
src/html.rs             HTML mail → egui widgets (subset: blocks, lists, tables, links, inline styles)
src/blitz_render.rs     HTML mail → rasterized page via Blitz (headless render, net provider, fonts)
src/css.rs              tiny CSS subset (decls, tag/class/id selectors) for HTML mail
src/folders.rs          folder kind (name → special mailbox, incl. CJK) + embedded row icons
src/render_bench.rs     test-only: times both HTML renderers for the same bodies (bench_render)
src/tray.rs             StatusNotifierItem over raw D-Bus (session bus, std::thread) + svg→ARGB icon
src/sync.rs             Job/Event enums, worker threads, per-protocol sync logic
src/protocol/mod.rs     protocol modules
src/protocol/stream.rs  TcpStream/TLS transport (MailStream, STARTTLS upgrade, buffered Conn)
src/protocol/ffdhe.rs   FFDHE kx group + DHE-RSA suites, which rustls does not ship
src/protocol/imap.rs    minimal IMAP4rev1 client (LOGIN, LIST, SELECT, FETCH w/ literals, UID STORE)
src/protocol/pop3.rs    minimal POP3 client (USER/PASS, STAT, UIDL, RETR w/ de-stuffing, QUIT)
```

### Data flow

```
UI (app.rs update)
   │  spawn(Job)                                   UI thread — no sockets, no DB writes
   ▼
worker thread (sync.rs)
   │  ImapClient / Pop3Client over MailStream      blocking I/O is fine here
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
  *first* fetch: the newest messages only, because everything older is
  deliberately out of scope — the mark is what says so. `Job::Sync` (startup
  and the title-bar button) fetches the folder list, INBOX and every folder that
  has been fetched before, and a folder that fails does not cost the others
  theirs (the first error is reported at the end). A folder the user never
  opened has no mark and waits for its first click, which is where its backfill
  happens; a warm folder with nothing new neither writes nor emits. The working
  mode (Preferences → General; `settings.json`'s `working_mode`, absent meaning
  the build) can switch the automatic half of that off: in **Debug** neither the
  startup sweep nor the fetch-on-first-open happens, the cached mail still loads
  from the database, and only the title-bar Sync button — which only Debug
  shows, since Release has nothing for it to add — and `Save & sync` in
  Preferences reach the server. It is a per-installation setting rather than a
  build flag so that a development build can be pointed at a real account
  without a sync on every start.
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
  held still for `BLITZ_SETTLE`, so a splitter drag or an animated window
  resize queues one render at the end instead of one per frame. Every request
  takes a fresh `blitz_render::Cancellation` ticket: a render whose ticket is
  stale gives up mid-layout (and between raster slices) and reports *nothing*,
  so a superseded render can never be mistaken for a failure of the newer one.
  Never render Blitz HTML on the UI thread.
- Searching is a job like any other: `Job::Search { seq, query }` runs
  `search::run` on a worker and answers `Event::SearchResults { seq, hits }`.
  Every request takes a new `seq`, and the UI drops an answer whose `seq` is not
  the current one, so a query typed over never shows the previous mail. The
  popup waits for the typing to stop (`SEARCH_SETTLE`, `search_settle`) — the
  title-bar box only displays the query — and Enter skips the wait;
  `search_applied` is the query the results on screen already answer, which is
  what keeps an unchanged query from searching again every frame. The request
  is pumped from `KimoApp::update`, not from the message list: the popup that
  edits the query sits outside every column.
- Remote mutations are limited to `UID STORE +FLAGS.SILENT (\Seen)` (IMAP).
  POP3 never deletes or mutates anything server-side.

### Storage layout

- `~/.local/share/kimo/mail.db` — the one database (SQLite, WAL, mode 0600
  because it holds the passwords). `accounts` (id, name, protocol, host,
  port, username, password, tls, position), `folders` (account_id, name,
  uidvalidity, uid_next, first_num, synced — the middle two are the sync marks
  of the bullet above), `messages` keyed by `(account_id, folder, uid)` with
  the subject, addresses, both bodies, the preview and the bigram column,
  `attachments` and `inline_images` (raw bytes, not base64) as children of a
  message, and the external-content FTS5 table `messages_fts`, which only the
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
  batch it has.
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
  `Job::Sync { purge }`, so nothing can reorder those two steps).
- `~/.config/kimo/settings.json` — UI settings: the theme
  (`system`/`light`/`dark`; "system" resolves via GTK/KDE config files, no
  subprocesses or D-Bus), `html_renderer` (`egui`/`blitz`, absent means
  `egui`), `working_mode` (`debug`/`release`, absent means the build — see the
  syncing bullet above), and the font families picked in Preferences →
  Appearance (`ui_font`, `mono_font`; absent means egui's built-in font plus
  the CJK fallback). Family *names* are stored, not paths, so the file survives font
  updates; a name that no longer resolves is simply ignored. Older files may
  still carry a `render_mode` field from the removed WebKitGTK renderer; it
  is ignored on load — do not reuse that name. The palette that the theme
  resolves to is *not* in this file; it is a compiled-in default, see the
  color-scheme bullet under Conventions.
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
  from protocol bugs.
- Protocol clients are synchronous and single-purpose; add a method only
  when a sync job actually needs it.
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
- egui is pinned to 0.31.x along with eframe. Keep both in lockstep when
  bumping, and expect API churn on upgrade (check the egui CHANGELOG).
- Layout is a three-column mail client: left navigation (collapsible via
  the title-bar toggle; auto-collapses when the window is narrower than
  640px — see `NAV_AUTO_COLLAPSE_WIDTH`), the central column (while a search is
  active the list shows `search_results` instead of the selected folder, and
  `message_card` paints both kinds of row), and the reading pane on the right
  with a "No mail selected" placeholder.
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
  them was noise the project owner asked to drop, and the magnifier turning
  accent-colored is the only sign that a query is what the column shows.
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
  `ROW_PREVIEW_FONT_SIZE` constants next to `MESSAGE_ROW_HEIGHT` (a 68 pt
  row whose three lines are centred at y = 20 / 40 / 57), so retuning the list
  is one edit instead of a hunt for literals.
- The window is undecorated on purpose (`with_decorations(false)` in
  `main.rs`): `titlebar()` in `app.rs` draws the custom title bar — the
  application icon (the brand, and the leftmost thing in the bar: the same
  embedded PNG the tray serves, downscaled once at startup because egui has
  no mipmaps to filter a 256 px source down to 18 pt), the nav collapse
  toggle, the icon-only sync button (Debug mode only: Release syncs on its
  own), the search box (a field with the magnifier inside it, one widget,
  whose click opens the search popup), the preferences cog, drag area,
  double-click maximize, and the
  minimize/maximize/close buttons sent as `ViewportCommand`s. Don't re-enable
  system decorations.
- `preferences_modal()` is the one place to configure Kimo: an
  `egui::Modal` (backdrop click or Escape closes it) with a sidebar of
  pages — **General** (the working mode), **Accounts** (account list with
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
  `ColorScheme::dark()`: surfaces (panel, window, card, hover, input, code,
  border), text and weak text, accent / selection / link, warn / danger /
  danger_hover, and the avatar palette — shared by both themes on purpose, so
  that a sender keeps its color when the theme changes.
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
  contrast floors (body text ≥ 4.5:1 on its panel, accent ≥ 3:1, text on the
  selection and hover fills ≥ 4.5:1, the close glyph on its red ≥ 4.5:1, both
  schemes' surfaces on opposite sides of the brightness scale); keep them green
  when you touch a color.
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
  under it. An account with no folders yet still offers INBOX, since clicking
  it is what starts the first fetch. Selecting a folder of another account is
  what makes it the active one.
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
  HTML bodies in the database. Note what the two numbers mean: the built-in
  renderer re-parses and re-lays out the body on *every* frame the reading
  pane is visible (there is no cache of the widget tree), while Blitz pays
  once per (message, pane width) on a worker thread.
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

- Attachments are listed by name but cannot be saved or opened yet.
- TLS finite-field DH covers the 2048-bit groups listed in
  `src/protocol/ffdhe.rs` and nothing else. A server whose DHE group is
  another size, or yet another custom prime, still fails the handshake with
  `SelectedUnofferedKxGroup`; see the FFDHE convention above for what to add.
- No message deletion/expunge (IMAP `\Deleted` + EXPUNGE, POP3 `DELE`).
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
- POP3 never deletes or mutates anything server-side, and a sync does not
  notice deletions either: mail removed in another client stays in the database
  and in the list forever. What a sync does is incremental by UIDL — a message
  already stored is never downloaded again — and bounded below by the
  `first_num` floor the first sync chose, so the messages a first sync skipped
  are never fetched, on purpose.
- Flags of mail that is already stored are not read back from the server. IMAP
  `\Seen` is taken for a message as it is fetched, and from then on the local
  flag is the only truth (`seen` only ever grows). Sync used to refresh the
  newest batch for free by refetching it; now that a warm sync fetches nothing
  it already has, mail read in another client keeps the unread mark it had here.
- No IDLE/push for IMAP; sync is manual or on-startup.
