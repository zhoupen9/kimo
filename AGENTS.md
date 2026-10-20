# AGENTS.md

Working agreements for AI agents (and humans) contributing to **Kimo**.
Read this before changing anything.

This file is deliberately short: it is loaded into every session, so it holds
only the rules, the commands and the map. The reasoning behind the code lives
in [`docs/`](docs/README.md), one file per area, and is read when that area is
touched.

## What this project is

Kimo is a desktop **email client** GUI written in pure Rust:

- **GUI:** egui (immediate mode, pinned to the 0.36 line), driven directly
  through winit + `egui-winit` + `egui-wgpu` instead of eframe, because the
  floating title bar's blur needs the frame split in two.
- **Protocols:** IMAP4rev1 and POP3 for reading, SMTP submission for sending —
  all implemented by hand in `src/protocol/`.
- **TLS:** rustls with the pure-Rust `ring` provider and webpki-roots, plus a
  hand-written finite-field DH group for the servers that offer nothing else.
- **Storage:** one SQLite database (FTS5) and one JSON file of UI settings.
  The bundled SQLite is the only C in the tree — see hard rule 1.
- **HTML mail:** two renderers, an in-egui subset and a Blitz raster. Remote
  images and stylesheets load by default; there is no JavaScript.
- **Desktop integration:** the file dialog is the desktop portal's over D-Bus,
  the tray is a hand-written StatusNotifierItem, notifications go to the
  freedesktop daemon — no GTK, no `zbus`, no async runtime.

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
src/wake.rs             the event loop's wake channel, for work that finishes while the window is hidden
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

## Where the detail lives

| Read this | Before touching |
| --------- | --------------- |
| [docs/architecture.md](docs/architecture.md) | `src/sync.rs`, `Job`/`Event`, anything crossing the UI/worker line |
| [docs/storage.md](docs/storage.md) | `src/db.rs`, `src/storage.rs`, `src/search.rs`, a schema migration, an XDG path |
| [docs/protocols.md](docs/protocols.md) | `src/protocol/`, a TLS setting, a charset, the mapping of a server's refusal |
| [docs/ui-shell.md](docs/ui-shell.md) | `src/main.rs`, `src/gpu.rs`, `src/glass.rs`, the title bar, the column plan, the navigation column |
| [docs/ui-mail.md](docs/ui-mail.md) | a list row, threading, select mode, the search box and its popup, the reading pane, attachments, labels, flags, filters, sort |
| [docs/ui-style.md](docs/ui-style.md) | drawing a widget: an icon or a disclosure, a mail address field, the Cc/Bcc fold |
| [docs/compose.md](docs/compose.md) | `compose_card`, `src/outgoing.rs`, `Job::SendMessage` |
| [docs/theme.md](docs/theme.md) | `src/theme.rs`, `src/fonts.rs`, any color a widget draws |
| [docs/html-mail.md](docs/html-mail.md) | `src/html.rs`, `src/css.rs`, `src/blitz_render.rs`, a Blitz build feature |
| [docs/platform.md](docs/platform.md) | `src/portal.rs`, `src/dbus.rs`, `src/tray.rs`, `src/notify.rs`, `packaging/` |
| [docs/preferences.md](docs/preferences.md) | `preferences_modal`, a `settings.json` key, the sync clock |
| [docs/testing.md](docs/testing.md) | before claiming a UI or renderer change works, or when timing one |
| [docs/limitations.md](docs/limitations.md) | before "fixing" behaviour that looks wrong — it may be a decision |

## Conventions that are rules

- Pure parsing helpers (e.g. `extract_uid`, `literal_size`, `html_to_text`)
  live next to their users and get `#[cfg(test)]` unit tests — that is where
  tests go in this codebase; no `tests/` directory yet.
- When touching the UI, actually run `cargo run` and click through it; the
  layout has no golden tests. A rendered dump is not the app — the dumps and
  the benchmarks are in `docs/testing.md`.

## Where new detail goes

Each doc under `docs/` is written the same way: the decision, the reason, and
the trap it cost. When a change is done, put what it taught you in the right
place:

- rationale, a rejected alternative, a wire-format gotcha → the matching
  `docs/*.md`, beside the feature it belongs to (not appended to the end);
- something an agent must not do, or a command to run → a rule in this file;
- a limitation accepted on purpose → `docs/limitations.md`.

Do not grow this file with feature prose. It has to stay small enough that the
tooling which loads it always does: a rule that is not here because the file
was too big to load is a rule that does not exist.
