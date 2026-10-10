# AGENTS.md

Working agreements for AI agents (and humans) contributing to **Jimao**.
Read this before changing anything.

## What this project is

Jimao is a desktop **email client** GUI written in pure Rust:

- **GUI:** eframe/egui (immediate mode, pinned to the 0.31 line for now).
- **Protocols:** IMAP4rev1 and POP3 **only**, implemented by hand in
  `src/protocol/` — no `imap`/`async-imap`/`pop` crates.
- **TLS:** rustls with the pure-Rust `ring` crypto provider + webpki-roots.
  The `ring` provider is installed process-wide in `src/main.rs`.
- **HTML mail:** parsed with html5ever (via `scraper`) and rendered as egui
  widgets in `src/html.rs`, with a small CSS subset (`src/css.rs`). Remote
  images and stylesheets load **by default** on worker threads (see the
  conventions below). There is no JavaScript. A WebKitGTK "web view" renderer
  lives in `src/webview.rs` behind the non-default `webview` feature (the
  documented exception to rule 1) and is picked from the titlebar; a plain
  `cargo build` stays pure Rust.
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
  stay `a(iiay)` — hosts cannot read a bare byte array.
- **Persistence:** JSON files under XDG dirs (`~/.config/jimao/accounts.json`,
  `~/.local/share/jimao/cache/<account>.json`). No external database.

## Hard rules

1. **Pure Rust only.** Never add a dependency that links C/C++ code or
   requires a system library to build: `native-tls`, `openssl-sys`,
   `aws-lc-rs`, `keyring`, etc. are all unacceptable. TLS goes through
   `rustls` (keep `default-features = false` + the `ring` feature). If a
   candidate crate needs pkg-config/Make/cmake at build time, it is wrong
   for this project. The single exception is the optional `webview` feature
   (`gtk` + `webkit2gtk`): it is off by default, so plain `cargo
   build`/`test`/`clippy` stay pure Rust, and only
   `cargo build --features webview` links C libraries and needs the
   webkit2gtk-4.1 + gtk+-3.0 development packages.
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
src/app.rs              all UI: JimaoApp state, panels, add-account window, event handling
src/models.rs           Account / Protocol / TlsMode / MailMessage (serde types)
src/error.rs            MailError + MailResult
src/storage.rs          XDG paths, accounts.json, per-account FolderCache/AccountCache
src/mail.rs             raw RFC 5322 bytes → MailMessage (mail-parser; HTML kept for rendering)
src/html.rs             HTML mail → egui widgets (subset: blocks, lists, tables, links, inline styles)
src/css.rs              tiny CSS subset (decls, tag/class/id selectors) for HTML mail
src/tray.rs             StatusNotifierItem over raw D-Bus (session bus, std::thread) + svg→ARGB icon
src/webview.rs          optional WebKitGTK mail window (feature `webview`, off by default)
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
  `~/.local/share/jimao/cache/resources/` (SHA-1 of the URL) and held in
  `JimaoApp::images`/`stylesheets`. The renderer reports missing URLs via
  `html::HtmlContext`; never fetch on the UI thread.
- `FETCH_LIMIT` (in `sync.rs`) caps how many of the newest messages are
  fetched per folder per sync.
- Remote mutations are limited to `UID STORE +FLAGS.SILENT (\Seen)` (IMAP).
  POP3 never deletes or mutates anything server-side.

### Storage layout

- `~/.config/jimao/accounts.json` — the account list, **including passwords,
  in plain text**. This is a documented, accepted limitation for now; do not
  silently change the format, and do not add a secrets daemon (see rule 1).
- `~/.config/jimao/settings.json` — UI settings: the theme
  (`system`/`light`/`dark`; "system" resolves via GTK/KDE config files, no
  subprocesses or D-Bus) and `render_mode` (`simple` in-app renderer, or
  `web` for the feature-gated WebKitGTK window).
- `~/.local/share/jimao/cache/<account-id>.json` — `AccountCache` with one
  `FolderCache` per folder (name, uidvalidity, synced flag, messages).
  Messages carry inline `cid:` images as base64, so caches of image-heavy
  mail can grow to a few MB.

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
  toggle, account menu (switch/add/remove), sync button, theme switcher
  (system/light/dark, persisted in `settings.json`), the HTML mail
  renderer toggle (simple ↔ web view; `webview` builds only), drag area,
  double-click maximize, and the minimize/maximize/close buttons sent as
  `ViewportCommand`s. Don't re-enable system decorations.
- When touching the UI, actually run `cargo run` and click through it; the
  layout has no golden tests.

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
  cache). A browser-grade renderer does exist, but only with
  `--features webview`: WebKitGTK renders in its own window (it cannot be
  embedded in the egui window on Wayland), reached through the titlebar
  toggle.
- POP3 sync re-downloads bodies every time (UIDL lets us skip but is not
  wired through the Job yet).
- No IDLE/push for IMAP; sync is manual or on-startup.
