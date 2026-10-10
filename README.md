# Kimo

A desktop email client written in **pure Rust**, with an immediate-mode GUI
built on [eframe/egui](https://github.com/emilk/egui). Kimo speaks exactly
two mail retrieval protocols: **IMAP4rev1** and **POP3** — both implemented
by hand in `src/protocol/` on top of `std::net::TcpStream` + rustls. There is
no C library in the dependency tree: TLS uses rustls with the `ring` crypto
provider and Mozilla's root certificates.

> Kimo is receive-only by design: there is no SMTP, so composing or sending
> mail is out of scope.

## Features

- Multiple accounts, IMAP and POP3
- Implicit TLS, STARTTLS, and plain connections
- Folder listing (IMAP), folder selection and per-folder caches
- Message list with unread markers, text preview, attachment names
- `\Seen` flag synchronization (IMAP server-side, POP3 local-only)
- Offline-first: messages are cached as JSON under
  `~/.local/share/kimo/cache/`; accounts live in `~/.config/kimo/accounts.json`
- CJK font fallback when a system Noto/WenQuanYi font is installed

## Build & run

```sh
cargo run            # debug
cargo run --release  # release
```

Linux users need the usual GUI stack headers available at runtime
(libxkbcommon, Wayland/X11 client libraries); eframe pulls everything else.

## Security note

Account credentials are currently stored in **plain text** in
`~/.config/kimo/accounts.json`. Treat that file accordingly.

## Development

See [AGENTS.md](AGENTS.md) for architecture notes, conventions and the
project's hard rules before contributing.
