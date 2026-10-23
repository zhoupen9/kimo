# Protocols — IMAP, POP3, SMTP and TLS

Read when touching `src/protocol/`, `src/outgoing.rs`, a TLS setting, a charset, or the mapping of a server's refusal onto `MailError`.

- **Protocols:** IMAP4rev1 and POP3 for reading, SMTP submission for
  sending, all implemented by hand in `src/protocol/` — no `imap`/`async-imap`/
  `pop`/`lettre` crates.

- **TLS:** rustls with the pure-Rust `ring` crypto provider + webpki-roots.
  `src/protocol/ffdhe.rs` adds the finite-field DH rustls does not ship (an
  FFDHE group and the two GCM `DHE-RSA` suites, the modular exponentiation
  coming from `crypto-bigint`), because some mail servers — Sina's, for one —
  offer TLS 1.2 with nothing but `DHE-RSA` and are otherwise unreachable.
  That provider is installed process-wide in `src/main.rs`.

- **Charsets:** mail-parser is built with the `full_encoding` feature
  (pulling in `encoding_rs`, pure Rust) so GB2312/GBK/GB18030/Big5/
  Shift-JIS/… mail decodes to proper UTF-8 instead of Latin-1 mojibake.

## The provider layer

- The three clients above are reached through `src/provider/` and not from
  `sync.rs` directly: `MailProvider` is the mailbox interface (IMAP and POP3
  adapters, one `open` per job), and `src/provider/vendor.rs` says where the big
  providers' servers are. Everything this page describes about the wire stays
  true underneath it; see [providers.md](providers.md).

## Sync, mutations and errors

- Remote mutations are `UID STORE +FLAGS.SILENT (\Seen)` (IMAP), `UID STORE
  ±FLAGS.SILENT (\Flagged)` for the reader's flag (IMAP too; a POP3 account has no
  server-side flags and keeps the flag locally), and, behind
  the title bar's Delete button, `\Deleted` + `UID EXPUNGE` (IMAP) or `DELE`
  (POP3). A delete tells the server first and deletes locally after; a message
  the server no longer has is deleted locally only, and a select-mode selection
  is one connection carrying every mail of one folder. See the deletion bullet in
  [ui-mail.md](ui-mail.md) for the whole rule.

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

## Finite-field DH

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

## Mailbox names

- IMAP mailbox names stay in their wire form (possibly modified UTF-7) in
  `Event`s, state and the database; `imap::decode_mailbox_name` decodes them for
  display only. SELECT/UID STORE must always use the wire form — do not
  re-encode decoded names by hand.
