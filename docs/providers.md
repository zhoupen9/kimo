# Providers — the mailbox interface and the vendor layer

Read when touching `src/provider/`, the account form's Provider row, or adding a mailbox operation or a vendor preset.

## The stack

```
UI / sync jobs                src/app.rs, src/sync.rs
  └─ vendor policy            src/provider/vendor.rs
       └─ MailProvider trait  src/provider/mod.rs
            ├─ ImapProvider   src/provider/imap.rs
            └─ Pop3Provider   src/provider/pop3.rs
                 └─ wire      src/protocol/{imap,pop3,smtp}.rs
```

- **The adapters own the wire; `sync.rs` owns the store.** `MailProvider`
  returns plain data and never opens `crate::db`, sends an `Event` or
  writes a file. `sync.rs` reads the marks, stores the batch and emits the
  events, in the order the batch asks for. The split is deliberate: it is what
  lets a POP3 fetch be tested against a local `TcpListener` with no
  database, and it keeps the transaction rules (which row goes first, which
  mark is written around which save) in the one place they were reasoned
  about.

- **The mark order is part of the answer.** `FolderBatch::marks_before_store`
  is `true` for POP3 — its floor bounds the next fetch and nothing rotates it,
  so the floor goes first and a crash can cost a repeated pass but never a
  message — and `false` for IMAP, where `db::save_messages` clears both
  marks when the UIDVALIDITY moved and the new ones have to be written after
  it. Do not unify the order in `sync.rs`: the two sequences are each
  protocol's own trap.

- **One session per job.** `provider::open` connects, negotiates TLS and logs
  in; `provider::with` runs one closure and logs out on success and failure
  alike, which is the connection hygiene `with_imap` used to carry by hand.
  A POP3 `delete` sends `QUIT` *inside* the call, because that is what commits
  the deletions; the provider tracks it so the `logout` that follows is not a
  second `QUIT`.

- **A capability is what lets sync skip a round trip it knows is impossible**,
  not a feature toggle. `folders` (IMAP) is why POP3 writes no folder list
  and emits no `Event::Folders`; `server_flags` is why mark-seen and
  flag only connect for IMAP; `append` is why a POP3 account never tries to
  file a sent copy; `stored_uids` is why only POP3's fetch is handed the
  UIDLs the database holds (IMAP's UID range makes that lookup unnecessary).

- **SMTP submission is not on the trait.** `Job::SendMessage` still reaches
  `protocol::smtp` directly, with the endpoint the vendor profile fills in.
  A submission session is a different socket from the read session, and both
  adapters would implement the same body — an interface with one shared
  implementation is ceremony, not a seam. The vendor layer still names the
  submission host, because that is where the provider's knowledge belongs.

## The vendor layer

The profiles are in `src/provider/vendor.rs`. A profile is data — endpoints,
sign-in kind, a sentence for the form, and the mailbox names a sent copy
prefers — because that is all the providers differ in today; a vendor that
needs a wire quirk gets a function there, passed the profile and the folder
list.

| Vendor | Inferred from | IMAP | SMTP | Sign-in |
| ------ | ------------- | ---- | ---- | ------- |
| Custom | anything else | — | — | the account's password |
| Gmail | `*.gmail.com`, `*.googlemail.com` | imap.gmail.com:993 TLS | smtp.gmail.com:465 TLS | app password |
| Outlook | `*.outlook.com`, `*.office365.com`, `*.office.com` | outlook.office365.com:993 TLS | smtp.office365.com:587 STARTTLS | OAuth2 only — cannot sign in |
| iCloud | `*.mail.me.com`, `*.icloud.com` | imap.mail.me.com:993 TLS | smtp.mail.me.com:587 STARTTLS | app-specific password |
| Yahoo | `*.yahoo.com`, `*.yahoo.co.jp` | imap.mail.yahoo.com:993 TLS | smtp.mail.yahoo.com:465 TLS | app password |
| QQ | `*.qq.com`, `*.foxmail.com` | imap.qq.com:993 TLS | smtp.qq.com:465 TLS | authorization code |
| NetEase | `*.163.com`, `*.126.com`, `*.yeah.net` | imap.163.com:993 TLS | smtp.163.com:465 TLS | client authorization code |

- **A preset can be switched off, and no server is switched off with it.**
  Preferences → Accounts has one switch per vendor (every `VendorKey` except
  Custom, all on by default), persisted as `disabled_vendors` in
  `settings.json`. A switched-off vendor is Custom everywhere: the form stops
  offering it (a form that was open on it falls back to Custom), and the send
  job is handed the *effective* key, so the Sent copy is chosen by the generic
  classifier instead of `[Gmail]/Sent Mail`. The account's endpoints, its
  cached mail and its sync do not change — disabling a preset is not disabling
  IMAP. `Custom` is not a vendor and has no switch. A key this build does not
  know is ignored, and a vendor added later starts switched on. The window owns
  the setting, so it passes the effective `VendorKey` with
  `Job::SendMessage`; the worker never reads `settings.json`.

- **Vendor identity is inferred, not stored.** `VendorKey::infer` reads the
  incoming host, then the outgoing one, and a host the table does not know is
  `Custom` — which is exactly the old behaviour: the folder-name heuristics
  (`folders::kind_of`) and the account's own fields. That is why there is no
  `vendor` column and no new field on `Account`: a schema migration and every
  `Account` literal would be a lot of churn for a value that is already a pure
  function of the endpoint. `VendorKey::of` is the seam if a vendor ever needs
  an override inference cannot express; the per-vendor switch above is the one
  override the app has today, applied through `vendor::effective_key`.

- **A preset fills the form; it gates nothing.** Picking Gmail sets the protocol
  to IMAP, the two hosts/ports/security modes and turns Sending on; Custom
  leaves whatever was typed alone. Outlook's note says Kimo cannot sign in
  because Microsoft requires OAuth2 (hard rule 4) — the form still lets the
  account be saved, because a server that is not quite what its host suggests
  is the user's to configure, and the server's refusal is the honest answer.

- **The Sent mailbox prefers the provider's own name** before the
  Sent-classified fallback, so a user folder called "Sent" cannot win over
  `[Gmail]/Sent Mail` or `Sent Items`. The answer is the wire form (possibly
  modified UTF-7), which is what `APPEND` has to be given.

## Adding things

- **A mailbox operation** is a method on `MailProvider` with both adapters
  implementing it, plus the store/event half in `sync.rs`. If only one
  protocol has the operation, the other answers a no-op or a
  `MailError::Protocol` and the caller checks `Capabilities` first; do not
  put a `match` back in a job.

- **A vendor** is a `VendorKey` variant, its profile (endpoints, sign-in, note,
  preferred Sent names) and the inference hosts. Nothing else changes unless
  the vendor needs a wire quirk; then it is a function in `vendor.rs` called
  from the one place that needs it.

- **A backend** (a protocol other than IMAP/POP3) is an implementation of
  `MailProvider` plus a `Protocol` variant and its database CHECK value —
  a change of its own, because the schema is where the protocol is stored. The
  trait is the seam; nothing in `sync.rs` has to learn about it.

## Limits and extension points

- No OAuth2/XOAUTH2 (hard rule 4): Outlook/Microsoft 365 therefore cannot sign
  in, and the form says so.
- No `X-GM-LABELS`, no IMAP `ID` handshake (some Chinese providers ask for
  it), no special-use `LIST` attributes — the vendor layer is where each
  would go, and each is a decision to make against a real server.
- Syncing is still the sweep it was: this layer moved the protocol knowledge,
  it did not add IDLE, push, or a provider-driven sweep.
