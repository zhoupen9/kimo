# Kimo

A desktop email client written in **pure Rust**, with an immediate-mode GUI
built on [egui](https://github.com/emilk/egui) — driven directly through winit,
`egui-winit` and `egui-wgpu`. Kimo speaks **IMAP4rev1** and **POP3** for
reading mail and **SMTP** (submission) for sending it, all implemented by hand
in `src/protocol/` on top of `std::net::TcpStream` + rustls. The dependency
tree has no C library beyond the bundled SQLite the mail database is: TLS uses
rustls with the `ring` crypto provider and Mozilla's root certificates.

> Sending is text-only submission: no attachments, no drafts, no outbox and no
> HTML bodies. The compose card writes a plain-text mail, sends it over the
> account's SMTP server and files a copy in the account's Sent mailbox when the
> server has one.

## Features

- Multiple accounts, IMAP and POP3
- Implicit TLS, STARTTLS and plain connections, for reading and for sending
- Folder listing (IMAP), folder selection and per-folder caches
- Message list with unread markers and text preview, grouped into
  conversations: one row per thread — its newest mail, how many it holds and a
  paperclip — with the chevron at its left opening the thread's mails in the
  list and a click on the row showing the whole conversation in the reading
  pane, one card per mail (grouped by `References` and `In-Reply-To`, or by
  subject for mail cached before Kimo kept those)
- A paperclip in the message list for mail that has attachments, and a card
  each under the mail itself: the file name (a link that downloads the file into
  `~/Downloads`), its MIME type and its size — the bytes themselves stay on the
  server, so a download fetches the mail again, and Export writes the whole
  message out as one `.eml`
- `\Seen` flag synchronization (IMAP server-side, POP3 local-only)
- Compose and send over SMTP, with the copy filed in Sent (IMAP `APPEND`)
- Export the open mail as the server's original `.eml`
- Search over the cached mail (SQLite FTS5 with a CJK bigram index), and two
  HTML renderers: native egui widgets, or a Blitz-rasterized page
- Offline-first: accounts and messages live in one SQLite database,
  `~/.local/share/kimo/mail.db`; UI preferences in
  `~/.config/kimo/settings.json`
- CJK font fallback when a system Noto/WenQuanYi font is installed

## Build & run

```sh
cargo run            # debug
cargo run --release  # release
```

Linux users need the usual GUI stack headers available at runtime
(libxkbcommon, Wayland/X11 client libraries).

## Security note

Account passwords — the incoming one and the outgoing one — are stored in
**plain text** in `~/.local/share/kimo/mail.db` (mode 0600). Treat that file
accordingly.

## Development

See [AGENTS.md](AGENTS.md) for architecture notes, conventions and the
project's hard rules before contributing.
