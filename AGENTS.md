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
  all implemented by hand in `crates/kimo-protocol/`.
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
   bodies, and DKIM or any signing. The *built-in* SMTP path authenticates
   with `AUTH PLAIN`/`AUTH LOGIN` over TLS only. OAuth2/XOAUTH2 is decided
   and allowed **for plugin providers** (the Gmail module; browser flow in
   `src/plugin/auth.rs`) — do not add it to the built-in clients.
5. **Keep the dependency tree small.** Every new crate needs a concrete
   justification; prefer std where reasonable.
6. **UI strings are English.** CJK rendering is handled by the best-effort
   font fallback in `fonts.rs::install` — do not add an i18n
   framework.

## Commands

| Task          | Command                                          |
| ------------- | ------------------------------------------------ |
| Build         | `cargo build`                                    |
| Run           | `cargo run -p kimo`                              |
| Tests         | `cargo test`                                     |
| Lint (strict) | `cargo clippy --all-targets -- -D warnings`      |
| Format        | `cargo fmt`                                      |

Everything must be green (build, tests, clippy, fmt) before you call work
done. There is no CI yet — that discipline is on you.

## Architecture

Kimo is one Cargo workspace: the binary at the root, five library crates under
`crates/`, and a separate workspace for the WASM modules.

```
crates/kimo-core/       Account / Protocol / TlsMode / MailMessage (plain types),
                        MailError, XDG paths, settings.json, resource cache, Mailbox/Folder
crates/kimo-mail/       raw RFC 5322 → MailMessage, outgoing → RFC 5322, conversations,
                        folder kinds (name → special mailbox, incl. CJK)
crates/kimo-protocol/   TcpStream/TLS transport, FFDHE, IMAP4rev1, POP3, SMTP submission
crates/kimo-store/      the SQLite database (schema, migrations, accounts, folders, messages,
                        labels, old-JSON import) and FTS5/CJK mail search
crates/kimo-plugin-abi/ the host-side JSON envelope + manifest (guest modules still carry
                        their own import/envelope module until the SDK extraction)
plugins/                separate workspace: gmail, outlook, sender-groups (wasm32-unknown-unknown)

src/main.rs             entry point; the winit event loop, the window, egui's input state
src/gpu.rs              the wgpu side: device, surface, egui's renderer, and one frame
src/gpus.rs             the machine's graphics cards and the Vulkan drivers a pick may load
src/app.rs              app root: KimoApp state and shared models, construction, one-frame `ui`
src/app/                UI by concern: events, actions, titlebar, navigation, list, reading,
                        compose, preferences, menus, search, blitz, layout, widgets, icons, util
src/fonts.rs            installed-font discovery (ttf-parser scan) + egui font installation
src/theme.rs            theme choice + the light/dark color schemes and their egui Visuals
src/portal.rs           the desktop's own file dialog, asked for over D-Bus (XDG portal)
src/html.rs             HTML mail → egui widgets (subset: blocks, lists, tables, links, inline
                        styles; the mail's colors adapted to the theme)
src/blitz_render.rs     HTML mail → rasterized page via Blitz (headless render, net provider, fonts)
src/css.rs              tiny CSS subset (decls, tag/class/id selectors, CSS colors) for HTML mail
src/app/icons.rs        embedded folder row icons and the other shared glyph painters
src/glass.rs            the floating title bar's frosted backdrop (wgpu passes + a paint callback)
src/render_bench.rs     test-only: times both HTML renderers for the same bodies (bench_render)
src/dbus.rs             the session bus: D-Bus wire protocol (marshalling, framing, the connection)
src/tray.rs             StatusNotifierItem over the session bus (std::thread) + svg→ARGB icon
src/wake.rs             the event loop's wake channel, for work that finishes while the window is hidden
src/notify.rs           new-mail notifications over the session bus (std::thread) + their 48·scale icon
src/sync.rs             Job/Event enums, worker threads, and the database/Event side of every mailbox job
src/plugin/             the WASM plugin host: discovery, the wasmi instance, the HTTP/TLS backend,
                        the OAuth browser flow, the MailProvider adapter (ABI in kimo-plugin-abi)
src/provider/mod.rs     the abstract MailProvider interface: one adapter per read protocol
src/provider/imap.rs    the IMAP adapter: folders, incremental fetch, flags, delete, append
src/provider/pop3.rs    the POP3 adapter: the one drop, older mail, delete, body
src/provider/vendor.rs  the vendor layer: Gmail/Outlook/… presets, sign-in notes, Sent mailbox
```

## Where the detail lives

| Read this | Before touching |
| --------- | --------------- |
| [docs/architecture.md](docs/architecture.md) | `src/sync.rs`, `Job`/`Event`, anything crossing the UI/worker line |
| [docs/storage.md](docs/storage.md) | `crates/kimo-store/`, `crates/kimo-core/src/storage.rs`, a schema migration, an XDG path |
| [docs/protocols.md](docs/protocols.md) | `crates/kimo-protocol/`, a TLS setting, a charset, the mapping of a server's refusal |
| [docs/providers.md](docs/providers.md) | `src/provider/`, a vendor preset, the account form's Provider row, a new mailbox operation |
| [docs/mail-spec.md](docs/mail-spec.md) | `src/plugin/`, a provider or extension plugin under `plugins/`, the WASM ABI, an OAuth flow, an X-GM mapping |
| [docs/ui-shell.md](docs/ui-shell.md) | `src/main.rs`, `src/gpu.rs`, `src/gpus.rs`, `src/glass.rs`, the title bar, the column plan, the navigation column |
| [docs/ui-mail.md](docs/ui-mail.md) | a list row, threading, select mode, the search box and its popup, the reading pane, attachments, labels, flags, filters, sort |
| [docs/ui-style.md](docs/ui-style.md) | drawing a widget: an icon or a disclosure, a mail address field, the Cc/Bcc fold |
| [docs/compose.md](docs/compose.md) | `compose_card`, `crates/kimo-mail/src/outgoing.rs`, `Job::SendMessage` |
| [docs/theme.md](docs/theme.md) | `src/theme.rs`, `src/fonts.rs`, any color a widget draws |
| [docs/html-mail.md](docs/html-mail.md) | `src/html.rs`, `src/css.rs`, `src/blitz_render.rs`, a Blitz build feature |
| [docs/platform.md](docs/platform.md) | `src/portal.rs`, `src/dbus.rs`, `src/tray.rs`, `src/notify.rs`, `packaging/` |
| [docs/preferences.md](docs/preferences.md) | `preferences_modal`, a `settings.json` key, the sync clock |
| [docs/testing.md](docs/testing.md) | before claiming a UI or renderer change works, or when timing one |
| [docs/limitations.md](docs/limitations.md) | before "fixing" behaviour that looks wrong — it may be a decision |

## Conventions that are rules

- **App UI code belongs in its concern module under `src/app/`.** `app.rs` is
  the root: `KimoApp` and the shared state/models, construction and `ui`
  orchestration. Do not grow it with another feature; add or extend the module
  that owns the concern, and keep each module under roughly 3,000 lines.
- Pure parsing helpers (e.g. `extract_uid`, `literal_size`, `html_to_text`)
  live next to their users and get `#[cfg(test)]` unit tests. App UI tests
  live under `src/app/tests/`, grouped by concern; there is no top-level
  `tests/` directory.
- A new mailbox operation goes on `provider::MailProvider` with an IMAP and a
  POP3 implementation; `src/sync.rs` must not branch on `Protocol` to reach the
  wire. Vendor knowledge (endpoints, sign-in, Sent mailbox) goes in
  `src/provider/vendor.rs`, not in the account form or an adapter. A preset
  switched off in Preferences is Custom everywhere: ask `vendor::enabled` /
  `vendor::effective_key` rather than assuming a preset is on.
- When touching the UI, actually run `cargo run -p kimo` and click through it; the
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
