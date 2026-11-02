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
   (see `crates/kimo-engine/src/sync.rs`); results come back over
   `std::sync::mpsc`. Do not introduce tokio, async-std, or an executor.
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
   `crates/kimo-engine/src/plugin/auth.rs`) — do not add it to the built-in
   clients.
5. **Keep the dependency tree small.** Every new crate needs a concrete
   justification; prefer std where reasonable.
6. **UI strings are English.** CJK rendering is handled by the best-effort
   font fallback in `crates/kimo-style/src/fonts.rs::install` — do not add an
   i18n framework.

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

Kimo is one Cargo workspace: a thin binary at the root, ten library crates
under `crates/`, and a separate workspace for the WASM modules.

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
crates/kimo-style/      theme choice + light/dark color schemes and their egui Visuals;
                        installed-font discovery (ttf-parser scan) and egui font installation
crates/kimo-platform/   the session bus (marshalling, framing, connection), the XDG file
                        portal, StatusNotifierItem tray, notifications, the event-loop wake
                        channel and the machine's graphics cards/Vulkan driver selection
crates/kimo-render/     HTML mail → egui widgets (`html`), the small CSS subset (`css`), the
                        Blitz raster renderer (`blitz_render`), the shared HTTP resource helper,
                        and the test-only renderer benchmark
crates/kimo-engine/     `Job`/`Event`, worker threads, the database/Event side of every mailbox
                        job; `provider/` (MailProvider plus IMAP, POP3 and vendor) and
                        `plugin/` (WASM host, discovery, HTTP/TLS backend, OAuth browser flow)
crates/kimo-ui/         `KimoApp` and the UI by concern: events, actions, titlebar,
                        navigation, list, reading, compose, preferences, menus, search,
                        blitz, layout, widgets, icons, util; `glass` (title-bar backdrop);
                        `app/tests/` holds the UI tests
plugins/                separate workspace: gmail, outlook, sender-groups (wasm32-unknown-unknown)

src/main.rs             entry point; the winit event loop, the window, egui's input state
src/gpu.rs              the wgpu side: device, surface, egui's renderer, and one frame
src/trim.rs             the allocator policy set before the first large allocation
```

Dependency direction: `kimo-ui` sits above `kimo-engine`; both build on
`kimo-render` / `kimo-platform` / `kimo-style`, and the data crates are below
all of them. The root binary depends on `kimo-ui`, `kimo-engine`,
`kimo-platform` and `kimo-style`; it does not contain a second copy of the
application.

## Where the detail lives

| Read this | Before touching |
| --------- | --------------- |
| [docs/architecture.md](docs/architecture.md) | `crates/kimo-engine/src/sync.rs`, `Job`/`Event`, anything crossing the UI/worker line |
| [docs/storage.md](docs/storage.md) | `crates/kimo-store/`, `crates/kimo-core/src/storage.rs`, a schema migration, an XDG path |
| [docs/protocols.md](docs/protocols.md) | `crates/kimo-protocol/`, a TLS setting, a charset, the mapping of a server's refusal |
| [docs/providers.md](docs/providers.md) | `crates/kimo-engine/src/provider/`, a vendor preset, the account form's Provider row, a new mailbox operation |
| [docs/mail-spec.md](docs/mail-spec.md) | `crates/kimo-engine/src/plugin/`, a provider or extension plugin under `plugins/`, the WASM ABI, an OAuth flow, an X-GM mapping |
| [docs/ui-shell.md](docs/ui-shell.md) | `src/main.rs`, `src/gpu.rs`, `crates/kimo-platform/src/gpus.rs`, `crates/kimo-ui/src/glass.rs`, the title bar, the column plan, the navigation column |
| [docs/ui-mail.md](docs/ui-mail.md) | a list row, threading, select mode, the search box and its popup, the reading pane, attachments, labels, flags, filters, sort |
| [docs/ui-style.md](docs/ui-style.md) | drawing a widget: an icon or a disclosure, a mail address field, the Cc/Bcc fold |
| [docs/compose.md](docs/compose.md) | `compose_card`, `crates/kimo-mail/src/outgoing.rs`, `Job::SendMessage` |
| [docs/theme.md](docs/theme.md) | `crates/kimo-style/src/theme.rs`, `crates/kimo-style/src/fonts.rs`, any color a widget draws |
| [docs/html-mail.md](docs/html-mail.md) | `crates/kimo-render/src/html.rs`, `crates/kimo-render/src/css.rs`, `crates/kimo-render/src/blitz_render.rs`, a Blitz build feature |
| [docs/platform.md](docs/platform.md) | `crates/kimo-platform/src/portal.rs`, `crates/kimo-platform/src/dbus.rs`, `crates/kimo-platform/src/tray.rs`, `crates/kimo-platform/src/notify.rs`, `packaging/` |
| [docs/preferences.md](docs/preferences.md) | `preferences_modal`, a `settings.json` key, the sync clock |
| [docs/testing.md](docs/testing.md) | before claiming a UI or renderer change works, or when timing one |
| [docs/limitations.md](docs/limitations.md) | before "fixing" behaviour that looks wrong — it may be a decision |

## Conventions that are rules

- **App UI code belongs in its concern module under
  `crates/kimo-ui/src/app/`.** `app.rs` is the root: `KimoApp` and the shared
  state/models, construction and `ui` orchestration. Do not grow it with another
  feature; add or extend the module that owns the concern, and keep each module
  under roughly 3,000 lines.
- Pure parsing helpers (e.g. `extract_uid`, `literal_size`, `html_to_text`)
  live next to their users and get `#[cfg(test)]` unit tests. App UI tests
  live under `crates/kimo-ui/src/app/tests/`, grouped by concern; there is no
  top-level `tests/` directory.
- A new mailbox operation goes on `provider::MailProvider` with an IMAP and a
  POP3 implementation; `crates/kimo-engine/src/sync.rs` must not branch on
  `Protocol` to reach the wire. Vendor knowledge (endpoints, sign-in, Sent
  mailbox) goes in `crates/kimo-engine/src/provider/vendor.rs`, not in the
  account form or an adapter. A preset
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
