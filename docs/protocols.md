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

- The draft that generalizes this adapter layer into a capability-based,
  WASM-loaded provider ABI — including Gmail's `X-GM` extensions, Microsoft
  Graph and the GUI extension hooks — is [mail-spec.md](mail-spec.md). Its
  host/ABI lives in `src/plugin/`; the protocol modules are not converted to
  it yet, so this page and [providers.md](providers.md) still describe the
  code an account uses today.

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

## Actions
**IMAP (Internet Message Access Protocol)** and **POP3 (Post Office Protocol version 3)** are both standard protocols used by email clients to retrieve messages from a server, but they are designed with completely different philosophies.

POP3 is built for **offline, single-device downloading** (download-and-delete), while IMAP is built for **online, multi-device synchronization** (leave messages on the server and sync state).

---

### 1. Actions Supported in POP3

POP3 is a simple, "store-and-forward" protocol. Its feature set is strictly limited to downloading mail from the primary inbox to a local client. POP3 operates in sequential states (AUTHORIZATION $\rightarrow$ TRANSACTION $\rightarrow$ UPDATE).

* **Authentication:** Log in using username and password (`USER`, `PASS`, or secure SASL methods).
* **List / View Mailbox Stats:** Get an overview of messages, such as the total count and total size of messages in the inbox (`STAT`, `LIST`).
* **Retrieve Messages:** Download a specific message or its headers to the local machine (`RETR`, `TOP`).
* **Mark for Deletion:** Flag specific messages to be deleted from the server (`DELE`). Deletions typically only take effect when the session successfully closes (`QUIT`).
* **Reset Deletions:** Unmark any messages that were flagged for deletion during the current session (`RSET`).
* **Session Check:** Ensure connection is alive (`NOOP`).

*What POP3 **Cannot** Do:* It does not support creating, renaming, or deleting server-side folders, syncing read/unread status across multiple devices, searching server-side emails, or accessing anything other than the main `INBOX`.

---

### 2. Actions Supported in IMAP

IMAP is a complex, stateful protocol designed to manage emails directly on the server. Changes made on one device are instantly synchronized across all other connected devices.

* **Folder / Mailbox Management:**
* Create, delete, or rename folders/directories on the server (`CREATE`, `DELETE`, `RENAME`).
* Select or examine a specific mailbox folder to work within (`SELECT`, `EXAMINE`).
* Check folder status like unread counts or UID validity (`STATUS`).


* **Message Organization & Flagging:**
* Copy or move messages between different folders (`COPY`, `MOVE`).
* Set or clear message flags like `\Seen` (read/unread), `\Answered` (replied), `\Flagged` (starred), or `\Deleted` (`STORE`).


* **Selective Retrieval & Fetching:**
* Fetch specific parts of an email (e.g., download just the headers, text body, or a specific MIME attachment separately) rather than downloading the whole raw file (`FETCH`).


* **Server-Side Searching:**
* Search through emails on the server using parameters like sender, date, text content, or flags without downloading them (`SEARCH`).


* **Appending Messages:**
* Upload and save a new message (like a sent draft or an imported email) directly into a server folder (`APPEND`).


* **Real-Time Push Updates (`IDLE`):**
* Maintain an open connection loop where the server instantly notifies the client the moment a new email arrives or a status changes elsewhere.


* **Expunging:**
* Permanently remove all messages marked with the `\Deleted` flag from the current folder (`EXPUNGE`).



---

### Summary Comparison of Capabilities

| Action Capability | POP3 | IMAP |
| --- | --- | --- |
| **Where emails live** | Downloaded locally, usually deleted from server | Stored on server, synchronized across devices |
| **Multi-device sync** | ❌ No (Read status on Device A doesn't sync to Device B) | ✅ Yes (Read, replied, and deleted states sync everywhere) |
| **Folder management** | ❌ No (Inbox only) | ✅ Yes (Create, delete, rename folders/labels) |
| **Partial message download** | ❌ No (Downloads entire message + attachments) | ✅ Yes (Fetch headers, body, or attachments selectively) |
| **Server-side search** | ❌ No | ✅ Yes |
| **Real-time notifications** | ❌ No (Must poll/reconnect) | ✅ Yes (via `IDLE`) |
