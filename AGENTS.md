# AGENTS.md

Working agreements for AI agents (and humans) contributing to **Kimo**.
Read this before changing anything.

## What this project is

Kimo is a desktop **email client** GUI written in pure Rust:

- **GUI:** eframe/egui (immediate mode, pinned to the 0.31 line for now).
- **Protocols:** IMAP4rev1 and POP3 **only**, implemented by hand in
  `src/protocol/` — no `imap`/`async-imap`/`pop` crates.
- **TLS:** rustls with the pure-Rust `ring` crypto provider + webpki-roots.
  The `ring` provider is installed process-wide in `src/main.rs`.
- **HTML mail:** two renderers, switched from the title bar and remembered
  in `settings.json` (`html_renderer`).
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
  `assets/app.png` (48px, rasterized from the multi-color `assets/app.svg`
  — regenerate with any svg→png tool, e.g. `rsvg-convert -w 48 -h 48`),
  embedded with `include_bytes!` and decoded with the image crate, keeping
  its own colors (the purple circle works on any panel). Left click raises
  the window; the right-click menu is a minimal `com.canonical.dbusmenu`
  object at `/Menu` (Show / Quit) served by the same thread. Missing
  StatusNotifierWatcher disables the tray silently, and `IconPixmap` must
  stay `a(iiay)` — hosts cannot read a bare byte array. The bus is silent
  for hours at a time, so reads go through `read_full`: an idle read-timeout
  expiry must never end the thread (`read_exact` turned it into a fatal
  `WouldBlock`, and the icon vanished 30s after startup).
- **Persistence:** JSON files under XDG dirs (`~/.config/kimo/accounts.json`,
  `~/.local/share/kimo/cache/<account>.json`). No external database.

## Hard rules

1. **Pure Rust only.** Never add a dependency that links C/C++ code or
   requires a system library to build: `native-tls`, `openssl-sys`,
   `aws-lc-rs`, `keyring`, etc. are all unacceptable. TLS goes through
   `rustls` (keep `default-features = false` + the `ring` feature). If a
   candidate crate needs pkg-config/Make/cmake at build time, it is wrong
   for this project. (A WebKitGTK "web view" renderer used to exist behind
   an optional `webview` feature; it has been removed. Do not reintroduce
   C-library exceptions to this rule.)
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
   font fallback in `app.rs::install_cjk_font` — do not add an i18n
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
src/main.rs             entry point; installs rustls ring provider, launches eframe
src/app.rs              all UI: KimoApp state, panels, add-account window, event handling
src/models.rs           Account / Protocol / TlsMode / MailMessage (serde types)
src/error.rs            MailError + MailResult
src/storage.rs          XDG paths, accounts.json, per-account FolderCache/AccountCache
src/mail.rs             raw RFC 5322 bytes → MailMessage (mail-parser; HTML kept for rendering)
src/html.rs             HTML mail → egui widgets (subset: blocks, lists, tables, links, inline styles)
src/blitz_render.rs     HTML mail → rasterized page via Blitz (headless render, net provider, fonts)
src/css.rs              tiny CSS subset (decls, tag/class/id selectors) for HTML mail
src/tray.rs             StatusNotifierItem over raw D-Bus (session bus, std::thread) + svg→ARGB icon
src/sync.rs             Job/Event enums, worker threads, per-protocol sync logic
src/protocol/mod.rs     protocol modules
src/protocol/stream.rs  TcpStream/TLS transport (MailStream, STARTTLS upgrade, buffered Conn)
src/protocol/imap.rs    minimal IMAP4rev1 client (LOGIN, LIST, SELECT, FETCH w/ literals, UID STORE)
src/protocol/pop3.rs    minimal POP3 client (USER/PASS, STAT, UIDL, RETR w/ de-stuffing, QUIT)
```

### Data flow

```
UI (app.rs update)
   │  spawn(Job)                                   UI thread — no sockets, ever
   ▼
worker thread (sync.rs)
   │  ImapClient / Pop3Client over MailStream      blocking I/O is fine here
   ▼
Event (mpsc) ──► drain_events() at frame start ──► merge into AccountUiState
                                                    └► storage::save_cache
```

- Merge identity: IMAP messages are keyed by `(UIDVALIDITY, UID)` per folder
  (a UIDVALIDITY change clears the folder cache); POP3 messages are keyed by
  their UIDL string, and `\Seen` is local-only.
- Remote HTML-mail resources (images, stylesheets) load by default — an
  explicit decision by the project owner, accepting that this leaks the
  reader's IP and open timing to senders (tracking pixels). Fetches run as
  `Job::FetchImage`/`FetchStylesheet` on worker threads via ureq (pure
  Rust, rustls + ring), are capped in size, cached on disk at
  `~/.local/share/kimo/cache/resources/` (SHA-1 of the URL) and held in
  `KimoApp::images`/`stylesheets`. The renderer reports missing URLs via
  `html::HtmlContext`; never fetch on the UI thread.
- `FETCH_LIMIT` (in `sync.rs`) caps how many of the newest messages are
  fetched per folder per sync.
- The Blitz renderer is a `Job::RenderHtml` on a worker thread: it parses,
  styles, lays out and rasterizes the body, resolving every sub-resource
  through `blitz_render::MailNet` (a blocking `NetProvider` that reuses the
  on-disk resource cache, and understands `data:` and `cid:` URLs). The page
  comes back as `Event::HtmlRendered` with premultiplied RGBA tiles and link
  boxes, which `app.rs` uploads as textures. Pages are keyed by
  message + account/folder + quantized geometry, at most two are cached, and
  a result whose key is no longer pending is dropped. Never render Blitz HTML
  on the UI thread.
- Remote mutations are limited to `UID STORE +FLAGS.SILENT (\Seen)` (IMAP).
  POP3 never deletes or mutates anything server-side.

### Storage layout

- `~/.config/kimo/accounts.json` — the account list, **including passwords,
  in plain text**. This is a documented, accepted limitation for now; do not
  silently change the format, and do not add a secrets daemon (see rule 1).
- `~/.config/kimo/settings.json` — UI settings: the theme
  (`system`/`light`/`dark`; "system" resolves via GTK/KDE config files, no
  subprocesses or D-Bus) and `html_renderer` (`egui`/`blitz`, absent means
  `egui`). Older files may still carry a `render_mode` field from the
  removed WebKitGTK renderer; it is ignored on load — do not reuse that name.
- `~/.local/share/kimo/cache/<account-id>.json` — `AccountCache` with one
  `FolderCache` per folder (name, uidvalidity, synced flag, messages).
  Messages carry inline `cid:` images as base64, so caches of image-heavy
  mail can grow to a few MB.
- The project was renamed from Jimao, so a pre-rename `jimao` directory under
  either XDG root is moved to the `kimo` name the first time those paths are
  used (`storage::migrate_legacy_dir`); an existing `kimo` directory wins and
  failures are ignored. Drop that helper and `LEGACY_APP_DIR` once no
  installation can still predate the rename.

## Conventions

- Errors: return `MailResult<T>`; convert with `?` (From impls exist for
  io/rustls/serde_json). Map server auth refusals to `MailError::Auth` so the
  UI shows them distinctly from protocol bugs.
- Protocol clients are synchronous and single-purpose; add a method only
  when a sync job actually needs it.
- IMAP mailbox names stay in their wire form (possibly modified UTF-7) in
  `Event`s, state and caches; `imap::decode_mailbox_name` decodes them for
  display only. SELECT/UID STORE must always use the wire form — do not
  re-encode decoded names by hand.
- Pure parsing helpers (e.g. `extract_uid`, `literal_size`, `html_to_text`)
  live next to their users and get `#[cfg(test)]` unit tests — that is where
  tests go in this codebase; no `tests/` directory yet.
- egui is pinned to 0.31.x along with eframe. Keep both in lockstep when
  bumping, and expect API churn on upgrade (check the egui CHANGELOG).
- Layout is a three-column mail client: left navigation (collapsible via
  the title-bar toggle; auto-collapses when the window is narrower than
  640px — see `NAV_AUTO_COLLAPSE_WIDTH`), central message list (avatar
  cards: sender + timestamp, subject, content preview — see
  `message_row`), right reading pane with a "No mail selected"
  placeholder.
- The window is undecorated on purpose (`with_decorations(false)` in
  `main.rs`): `titlebar()` in `app.rs` draws the custom title bar — nav
  toggle, account menu (switch/add/remove), sync button, HTML renderer
  switch (Simple/Blitz), theme switcher (system/light/dark, both persisted
  in `settings.json`), drag area,
  double-click maximize, and the minimize/maximize/close buttons sent as
  `ViewportCommand`s. Don't re-enable system decorations.
- When touching the UI, actually run `cargo run` and click through it; the
  layout has no golden tests. `blitz_render` has a developer tool for
  eyeballing fidelity without the GUI:
  `KIMO_RENDER_HTML=mail.html cargo test -- --ignored dump_render` writes
  `mail.png` next to the input.
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
- No message deletion/expunge (IMAP `\Deleted` + EXPUNGE, POP3 `DELE`).
- Credentials in plain text (see above); no OAuth2/XOAUTH2.
- HTML mail renders through the in-egui subset renderer (`src/html.rs`).
  CSS is the `src/css.rs` subset (colors, fonts, sizes, display/visibility);
  compound selectors and layout CSS are ignored. Simple tables render as
  aligned grids; layout tables (block elements inside cells — most HTML
  mail) render as flowing full-width rows, because egui Grid sizes columns
  to the minimum wrapped width. `cid:` inline images render from the
  attachment bytes stored on the message (`InlineImage`, base64 in the
  cache). Improving fidelity here means improving this renderer — Blitz mode
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
- POP3 sync re-downloads bodies every time (UIDL lets us skip but is not
  wired through the Job yet).
- No IDLE/push for IMAP; sync is manual or on-startup.
