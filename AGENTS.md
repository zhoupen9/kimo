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
- **Receive-only by design.** There is no SMTP and no mail composing/sending.
- **Persistence:** JSON files under XDG dirs (`~/.config/jimao/accounts.json`,
  `~/.local/share/jimao/cache/<account>.json`). No external database.

## Hard rules

1. **Pure Rust only.** Never add a dependency that links C/C++ code or
   requires a system library to build: `native-tls`, `openssl-sys`,
   `aws-lc-rs`, `keyring`, etc. are all unacceptable. TLS goes through
   `rustls` (keep `default-features = false` + the `ring` feature). If a
   candidate crate needs pkg-config/Make/cmake at build time, it is wrong
   for this project.
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
src/mail.rs             raw RFC 5322 bytes → MailMessage (mail-parser, HTML→text fallback)
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
- `FETCH_LIMIT` (in `sync.rs`) caps how many of the newest messages are
  fetched per folder per sync.
- Remote mutations are limited to `UID STORE +FLAGS.SILENT (\Seen)` (IMAP).
  POP3 never deletes or mutates anything server-side.

### Storage layout

- `~/.config/jimao/accounts.json` — the account list, **including passwords,
  in plain text**. This is a documented, accepted limitation for now; do not
  silently change the format, and do not add a secrets daemon (see rule 1).
- `~/.local/share/jimao/cache/<account-id>.json` — `AccountCache` with one
  `FolderCache` per folder (name, uidvalidity, synced flag, messages).

## Conventions

- Errors: return `MailResult<T>`; convert with `?` (From impls exist for
  io/rustls/serde_json). Map server auth refusals to `MailError::Auth` so the
  UI shows them distinctly from protocol bugs.
- Protocol clients are synchronous and single-purpose; add a method only
  when a sync job actually needs it.
- Pure parsing helpers (e.g. `extract_uid`, `literal_size`, `html_to_text`)
  live next to their users and get `#[cfg(test)]` unit tests — that is where
  tests go in this codebase; no `tests/` directory yet.
- egui is pinned to 0.31.x along with eframe. Keep both in lockstep when
  bumping, and expect API churn on upgrade (check the egui CHANGELOG).
- The window is undecorated on purpose (`with_decorations(false)` in
  `main.rs`): `titlebar()` in `app.rs` draws the custom title bar — drag
  area, double-click maximize, and the minimize/maximize/close buttons sent
  as `ViewportCommand`s. Don't re-enable system decorations.
- When touching the UI, actually run `cargo run` and click through it; the
  layout has no golden tests.

## Accepted limitations / roadmap

Do not "fix" these casually — they are known:

- Attachments are listed by name but cannot be saved or opened yet.
- No message deletion/expunge (IMAP `\Deleted` + EXPUNGE, POP3 `DELE`).
- Credentials in plain text (see above); no OAuth2/XOAUTH2.
- HTML bodies are rendered as tag-stripped text, not a web view (a webview
  would violate rule 1 — the realistic option is an in-egui HTML renderer,
  not webkit).
- POP3 sync re-downloads bodies every time (UIDL lets us skip but is not
  wired through the Job yet).
- No IDLE/push for IMAP; sync is manual or on-startup.
