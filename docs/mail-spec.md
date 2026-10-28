# Kimo mail provider and extension specification

**Status:** design draft, 2026-10-27. The host side is implemented in
`src/plugin/`: the ABI types, manifest validation, discovery, the WASM
instance, permissions, fuel and memory limits, extension-plan validation and a
`MailProvider` adapter. Preferences → Plugins lists what the scan found and
switches plugins on and off (a disabled module is not compiled at startup),
and the host has a real HTTP/TLS backend, manifest network rules are enforced
as an endpoint allowlist, [`plugins/sender-groups`](../plugins/sender-groups/)
is a minimal reference extension, [`plugins/gmail`](../plugins/gmail/) is
the first real provider: XOAUTH2, all four `X-GM-EXT-1` extensions, Gmail
submission, and an OAuth2 authorization-code flow whose browser leg the
host runs (`src/plugin/auth.rs`: loopback redirect, the desktop's OpenURI
portal, `xdg-open` fallback). [`plugins/outlook`](../plugins/outlook/) is
the second, over Graph: Entra ID OAuth2, POP3-shaped sync by immutable ids,
flags, soft delete, KQL search, and `sendMail` with its own Sent copy —
installed but off by default, per §8.8. The capability gates the host's
sync paths ask (`send.save_copy`, `message.set_flags`, `message.append`)
come from the module's own descriptor, and the Sent-copy rule of §5.3 is
enforced there: a module that files its own copy is never handed a second. The IMAP/POP3/SMTP/Graph reference
modules are not built yet, and the current client still reads IMAP/POP3
through `src/protocol/` — but a plugin-backed account is wired: the Gmail
module's provider is an entry in the account form (a dedicated entry per
installed provider), its OAuth2 sign-in runs as `Job::PluginSignIn`, its
tokens live on the account row with refresh on session open, and its
submission and Sent copy go through the module. The target specification version is 1.0; this file is the 0.1 review
copy.

Read this when designing or reviewing a provider plugin, an extension, a new
protocol mapping, or the migration from the current protocol clients.

The specification is deliberately a **superset** of the protocols Kimo already
speaks. A provider may implement only the small, standard subset it can support
and still be a conforming Kimo provider. The UI must never assume a capability
that the provider did not declare.

## 0. Scope and non-goals

**In scope**

- mailbox operations (the IMAP layer), message operations (the POP3 layer),
  submission (the SMTP layer), and a common envelope around all of them;
- provider-specific operations that no standard protocol defines: native
  labels, native threads, raw provider queries, extended authentication;
- Microsoft Graph (Outlook / Microsoft 365) and Gmail's `X-GM-EXT-1`
  extensions as pluggable providers;
- provider and extension plugins written in Rust, compiled to `.wasm`
  modules and loaded by Kimo;
- GUI extension points for mail organization and mail display.

**Out of scope for this draft**

- an IMAP/POP3/SMTP *server* — Kimo is a client;
- a plugin marketplace, automatic download or automatic update of plugins;
- native shared-library plugins (`.so`, `.dylib`, `.dll`); WebAssembly is
  the only plugin format;
- process isolation of plugins. The WASM sandbox is in-process; a separate
  plugin process is not part of v1;
- JMAP, calendar and contacts. JMAP can become another provider profile later
  without changing the ABI;
- an outbox, retry queue or draft persistence. The current hard rule 4 still
  stands: sending is SMTP submission, and nothing is queued or retried. This
  document adds an interface for submission, not a queue.

**Normative words.** *MUST*, *MUST NOT*, *SHOULD* and *MAY* are used in the
usual RFC 2119 sense. "The host" is Kimo. "Provider" means a mailbox or
submission implementation. "Extension" means a GUI contribution. "Plugin" is
the shared Rust `.wasm` module for either.

**Relationship to the current rules.** The hard rules in `AGENTS.md` still
apply to every implementation of this draft: pure Rust, no async runtime, no
work on the UI thread, and SQLite as the only C dependency. Loading a
`.wasm` module does not make it a C dependency: the interpreter is pure Rust.
OAuth2 and drafts are still owner decisions; this draft only makes room for
them in the interface.

---

## 1. The shape of the design

```
Kimo UI
  │  user action (read, label, move, delete, send)
  ▼
Host (Kimo)
  │  validates, chooses provider + local fallback, owns the database
  ├──────────────► local store (SQLite, FTS5)
  ├──────────────► provider plugin (.wasm) ──► IMAP / POP3 / SMTP / Graph / Gmail
  └──────────────► extension plugin (.wasm) ──► declarative view plan ──► UI
```

An **action** is a request about mail, not a protocol command. The same action
may be satisfied in three different ways:

| Layer | What it means | Example |
| --- | --- | --- |
| Local | The host changes its own database, because the semantics are local | A local label on a plain IMAP account |
| Protocol | The provider implements a standard protocol operation | IMAP `UID STORE +FLAGS (\Seen)` |
| Vendor | The provider uses a provider-specific operation | Gmail `X-GM-LABELS`, Graph categories |

The host is the only writer of the local store. A provider returns **effects**
— what should be stored, deleted, relabelled or re-threaded — and the host
validates and applies them. A provider never runs SQL, never touches an egui
context and never draws.

The provider is more than a transport: it owns the remote semantics. It does
not own the socket or TLS stack — the host provides a narrow TLS-stream or
HTTP capability, so a plugin can speak IMAP, POP3, SMTP or Graph without
ambient network access. The host also owns scheduling, the database,
credentials policy, endpoint allowlists and error presentation.

### 1.1 The subset relation

```
kimo-core/1   (mailbox providers: list, sync, list messages, fetch)
├── kimo-imap/1   (core + flags, delete, append, optional move/search/thread)
├── kimo-pop3/1   (core, one mailbox, delete, no server flags)
├── kimo-gmail/1  (imap + X-GM labels/threads/raw query + Gmail submission)
└── kimo-graph/1  (Graph mail: folders, messages, categories, conversations, send)

kimo-smtp/1    (submission only; no mailbox)
```

A provider that implements only POP3 is a valid `kimo-pop3/1` provider. A
provider that implements Gmail is a `kimo-imap/1` provider plus the Gmail
profile. Outlook is not an IMAP superset; it is a different provider profile
that implements the same Kimo operations through Graph. The UI sees the
operations, not the wire protocol.

---

## 2. Decisions at a glance

| # | Decision | Why |
| --- | --- | --- |
| 1 | The host owns the canonical local store; plugins never write SQLite | One schema, one migration story, one transaction boundary. A plugin cannot corrupt the database or hold a write lock. |
| 2 | Providers declare capabilities; conformance is a declared subset, not a guess | A POP3 account and a Graph account must both be usable without the UI probing for features. Missing capability means explicit fallback or "unsupported", never a silent protocol attempt. |
| 3 | Server-visible mutations are remote-first; local-only state is local-first | A mail deleted locally but still on the server comes back at the next sync. The current delete path already does remote-first for this reason. |
| 4 | No silent destructive fallback | "Move" cannot quietly become a local folder change, and "delete" cannot quietly become "hide". The user sees an error, not a lie. |
| 5 | The ABI is a versioned WebAssembly module with a JSON envelope | No C ABI, no `.so`, no compiler-specific function pointers. A `.wasm` is portable and can be validated before it runs. |
| 6 | Plugins run in a WASM sandbox with host-granted capabilities | A plugin gets no ambient filesystem, network, process or database access. It can use only the host imports the manifest declares, and the UI can show them. |
| 7 | Extensions return declarative view plans, not egui calls | The host can validate limits, keep theme/semantic colours, and keep the ABI stable. Raw widget callbacks would couple every extension to egui's internals. |
| 8 | Extensions never run on the UI thread | A WASM module can still burn its fuel budget; a frame must not wait for it. Hooks run on a worker and the host draws the last valid plan, or the built-in default. |
| 9 | Plugin instances are disposable; provider switches still wait for the next start | There are no native function pointers to keep alive. A provider toggle waits for the next start so an in-flight account job is not switched underneath the user; an extension toggle applies at the next hook. |
| 10 | A provider message may have a global identity and several mailbox locations | Gmail's `X-GM-MSGID` names one message under several labels; a `(folder, UID)` key would duplicate it. |

---

## 3. The data model

The shapes below are ABI-level shapes, not the final Rust structs. They are
serialised as JSON across the plugin boundary. Unknown fields MUST be ignored
for forward compatibility.

### 3.1 Account and submission

```rust
struct Account {
    id: AccountId,
    provider: ProviderId,              // "imap", "pop3", "gmail", "outlook", ...
    provider_config: Json,             // provider-specific, validated by the provider
    provider_config_version: u32,
    submission: Option<Submission>,    // None: the account cannot send
    enabled: bool,
}

struct Submission {
    provider: ProviderId,              // "smtp" for the standard profile, "outlook" for Graph
    config: Json,
}
```

Credentials are **not** part of `provider_config`. They live in a separate
credential record, are passed to the provider only for the duration of a call,
and are redacted from logs and errors.

```rust
enum Credential {
    Password { username: String, password: Secret },
    OAuth2 {
        access_token: Secret,
        refresh_token: Option<Secret>,
        expires_at: Option<Timestamp>,
        scope: Option<String>,
    },
}
```

The current database stores passwords in plain text (an accepted limitation,
see [storage.md](storage.md)). The interface does not make that better or
worse; it only keeps the secret out of `provider_config`, out of log lines and
out of error strings.

A mailbox provider and a submission provider are separate. The standard case
is `imap` + `smtp`; Gmail is `gmail` + `smtp` (or `gmail` for both);
Outlook is `outlook` for both. This is what makes "SMTP submission is a
subset" true without forcing every mailbox provider to implement sending.

### 3.2 Provider descriptor

```rust
struct ProviderDescriptor {
    id: ProviderId,
    name: String,
    version: String,                   // plugin version
    spec_version: String,              // "1.0"
    profiles: Vec<String>,             // "kimo-core/1", "kimo-imap/1", ...
    capabilities: Vec<Capability>,     // static capabilities
    auth: Vec<AuthMethod>,
    config: Vec<ConfigField>,          // rendered by the host's account form
    network: Vec<NetworkRule>,         // endpoints the host may open for this provider
    limits: Limits,
}

struct NetworkRule {
    transport: NetworkTransport,       // Tls | Http
    host: String,
    port: Option<u16>,
}

struct Capability {
    name: String,                      // "message.set_labels"
    params: Json,                      // {"dialect":"gmail"} for query.raw
}
```

`network` lists fixed endpoints (Gmail's servers, Graph's endpoints); a
generic provider leaves it empty and uses the account's configured host. The
host enforces the union: a plugin cannot open an endpoint that neither the
descriptor nor the account names.

A capability is a promise. A provider that declares it MUST implement it for
the account it described. A provider that does not declare it MUST return
`unsupported` if asked. Capabilities may be refined after connection (for
example, whether a Gmail server advertises `X-GM-EXT-1`); the host keeps an
**effective capability set** per account and uses that for dispatch.

```text
capability name        meaning
---------------------  ------------------------------------------------------
mailbox.list           enumerate mailboxes
mailbox.create         create a mailbox
mailbox.rename         rename a mailbox
mailbox.delete         delete a mailbox
mailbox.subscribe      change IMAP subscription state
mailbox.unsubscribe    change IMAP subscription state
mailbox.status         counts and sync marks for one mailbox
mailbox.expunge        expunge messages already marked deleted
mailbox.sync           incremental or full sync of one mailbox
message.list           list summaries, paged
message.fetch          fetch a message body, parts or raw source
message.fetch.raw      raw RFC 5322 source is available
message.fetch.parts    individual MIME parts are available
message.set_flags      server-side flags are writable
message.set_labels     native labels are writable
message.move           native move
message.copy           native copy
message.delete         native delete (soft or permanent as declared)
message.delete.permanent  permanent delete is available
message.append         append a raw message to a mailbox
message.archive        move to the provider's archive role
message.junk           mark junk or move to junk
message.move.emulate   copy + delete is a safe move fallback
message.search         server-side structured search
query.raw              provider query language, with params.dialect
thread.native          a native thread id is available
attachment.fetch       one attachment can be fetched alone
send.submit            submission is available
send.save_copy         submission can file its own Sent copy
send.dsn               delivery status notifications
send.smtputf8          SMTPUTF8
sync.delta             incremental sync with a cursor
sync.deletions         the provider reports remote deletions
auth.password          username/password
auth.oauth2.device_code
auth.oauth2.auth_code_pkce
auth.xoauth2
auth.oauthbearer
```

Profile required sets:

| Profile | Required capabilities | Notes |
| --- | --- | --- |
| `kimo-core/1` | `mailbox.list`, `mailbox.sync`, `message.list`, `message.fetch` | Every mailbox provider. |
| `kimo-imap/1` | core, plus `message.set_flags`, `message.delete`, `message.append` | `message.move`, `message.search`, `thread.native` optional. |
| `kimo-pop3/1` | core, plus `message.delete` | One mailbox; no server flags, labels, move or search. |
| `kimo-gmail/1` | `kimo-imap/1` plus `message.set_labels`, `thread.native`, `query.raw` (dialect `gmail`) | Degrades to plain IMAP if `X-GM-EXT-1` is absent. |
| `kimo-graph/1` | core, plus `message.set_flags`, `message.set_labels`, `message.move`, `message.delete`, `message.search`, `thread.native`, `query.raw` (dialect `graph-kql`), `send.submit` | Categories are labels; `conversationId` is the thread. |
| `kimo-smtp/1` | `send.submit` | Submission only; no mailbox operations. |

Host-owned local accounts (imported mail, not a network provider) are outside
this ABI: they use the host's local operations and have no remote capability
set. They are not a fourth standard protocol hiding in the profile table.

### 3.3 Mailboxes, messages and identity

```rust
struct Mailbox {
    id: MailboxId,                     // provider-stable: IMAP wire path, Graph folder id
    name: String,                      // display name
    parent: Option<MailboxId>,
    role: MailboxRole,                 // inbox, sent, drafts, trash, junk, archive, all, custom
    attributes: Vec<String>,           // \Noselect, \HasChildren, special-use, ...
    total: Option<u32>,
    unread: Option<u32>,
}

struct MessageIdentity {
    global: Option<String>,            // X-GM-MSGID, Graph immutable id; absent for IMAP
    mailbox: MailboxId,
    local: String,                     // IMAP UID, POP3 UIDL, Graph id
    generation: Option<String>,        // IMAP UIDVALIDITY, Graph change generation
}
```

A **message** is the mail itself; a **location** is where a mailbox says it is.
A plain IMAP message has one location per mailbox copy. A Gmail message has one
identity and one location per Gmail label it carries. A Graph message normally
has one location. The host maps these to one local row plus zero or more
locations.

```rust
struct MessageSummary {
    identity: MessageIdentity,
    thread: Option<ThreadId>,          // native or host-assigned
    subject: String,
    from: Vec<Address>,
    to: Vec<Address>,
    cc: Vec<Address>,
    date: Timestamp,
    size: Option<u64>,
    flags: Flags,
    labels: Vec<LabelId>,
    has_attachments: bool,
    snippet: Option<String>,
    provider_data: Json,               // opaque, provider-owned
}

struct Flags {
    seen: bool,
    flagged: bool,
    answered: bool,
    forwarded: bool,
    draft: bool,
    deleted: bool,
    junk: bool,
    not_junk: bool,
    keywords: BTreeSet<String>,        // IMAP keywords, provider extensions
}
```

```rust
struct FetchedMessage {
    summary: MessageSummary,
    raw: Option<BlobRef>,              // RFC 5322 source, if the provider has it
    body: Option<MessageBody>,         // structured form, e.g. Graph JSON
    parts: Vec<MessagePart>,
}

struct MessageBody {
    text: Option<String>,
    html: Option<String>,
    attachments: Vec<AttachmentMeta>,
    inline_images: Vec<InlineImageMeta>,
}

struct MessagePart {
    id: String,
    content_type: String,
    filename: Option<String>,
    disposition: Option<String>,
    content_id: Option<String>,
    size: u64,
    data: BlobRef,
}
```

If `raw` is present the host parses it with the existing `mail-parser` path in
`src/mail.rs`; if only `body` is present (Graph can return structured JSON),
the host normalises it into the same internal `MailMessage`. A provider MUST
NOT return both a raw source and a conflicting structured body.

```rust
struct Label {
    id: LabelId,
    name: String,
    origin: LabelOrigin,               // Local | Provider
    remote_id: Option<String>,         // Gmail label, Graph category string
    color: Option<String>,             // semantic token, not a raw hex value
    parent: Option<LabelId>,           // Gmail nested labels; Graph has none
}

enum LabelOrigin { Local, Provider }

struct Thread {
    id: ThreadId,
    subject: String,
    message_ids: Vec<MessageId>,
    latest: Timestamp,
    unread: u32,
    origin: ThreadOrigin,              // Native | Local
}
```

Label origin is important. A **local** label never leaves the machine. A
**provider** label is mirrored from Gmail or Graph and every mutation of it
goes back to the provider. If local and native labels share a name they remain
distinct records; the UI may draw one chip for the name but MUST keep the two
origins separate for routing and for "why is this label not on my phone?".

### 3.4 Queries and pages

```rust
struct Query {
    text: Option<String>,
    from: Option<String>,
    to: Option<String>,
    subject: Option<String>,
    since: Option<Date>,
    before: Option<Date>,
    mailbox: Option<MailboxId>,
    labels: Vec<LabelId>,
    flags: Option<FlagFilter>,
    has_attachment: Option<bool>,
    thread: Option<ThreadId>,
    raw: Option<RawQuery>,
}

struct RawQuery {
    dialect: String,                   // "gmail", "graph-kql", "imap"
    expression: String,                // interpreted only by the provider
}

struct Page<T> {
    items: Vec<T>,
    next_cursor: Option<SyncCursor>,
    total_estimate: Option<u32>,
}
```

Structured fields are portable. `raw` is not: it is a provider dialect and the
UI MUST label it as such. The host never concatenates a raw query into SQL,
a shell, or an IMAP command of its own; the provider owns the encoding and
MUST reject CR/LF and oversized input.

### 3.5 Operations, effects and errors

```rust
struct Operation {
    id: OperationId,
    account: AccountId,
    name: String,                      // "message.set_labels"
    target: Target,                    // account | mailbox | messages | thread | query
    params: Json,
    mode: Mode,                       // Native | Local | Auto
    deadline_ms: Option<u64>,
}

enum Mode {
    Native,                           // require the provider capability
    Local,                            // local store only; provider must not be called
    Auto,                             // native if available, safe fallback otherwise
}

struct OperationResult {
    effects: Vec<Effect>,
    result: Json,                     // operation-specific, e.g. send status
    warnings: Vec<Warning>,
    credentials: Option<CredentialUpdate>,
}

enum Effect {
    MailboxesUpsert(Vec<Mailbox>),
    MailboxesRemove(Vec<MailboxId>),
    MailboxesReset { mailbox: MailboxId, generation: Option<String> },
    MessagesUpsert(Vec<FetchedMessage>),
    MessagesRemove(Vec<MessageRemoval>),
    FlagsSet { identities: Vec<MessageIdentity>, flags: Flags },
    LabelsSet { identities: Vec<MessageIdentity>, labels: Vec<LabelId> },
    ThreadsSet { identities: Vec<MessageIdentity>, thread: Option<ThreadId> },
    CursorSet { mailbox: MailboxId, cursor: SyncCursor, generation: Option<String> },
}

struct MessageRemoval {
    identity: MessageIdentity,
    scope: RemovalScope,              // Location | Message
}

enum RemovalScope { Location, Message }

struct ProviderError {
    code: String,                     // unsupported, auth_failed, network, protocol, ...
    message: String,
    retryable: bool,
    retry_after_ms: Option<u64>,
    details: Json,
    cause: Option<String>,            // redacted chain
}
```

Effects are the only way a provider changes local state. The host validates
each effect against the account, the mailbox generation and the provider's
declared capabilities, then applies the whole list in one SQLite transaction.
An unknown effect or an invalid reference rejects the entire result; a
provider cannot smuggle a partial update through a warning.

---

## 4. What an operation means

The same operation may be native, local, or unsupported. The host resolves it
in this order:

1. `Mode::Local` — apply the local operation. Never call the provider.
2. `Mode::Native` — the effective capability must exist, or return
   `unsupported`. Never fall back.
3. `Mode::Auto` — use the native capability if it exists. Otherwise use the
   fallback in the table below **only if that fallback is semantically safe**.
   Otherwise return `unsupported`.

| Operation | Native capability | `Auto` fallback when absent |
| --- | --- | --- |
| `message.set_labels` | `message.set_labels` | Local label only. A label with origin `Provider` cannot be changed; the operation is `unsupported` for that label. |
| `message.set_flags` | `message.set_flags` | Local flags only. Correct for POP3; for an IMAP account the local mark would diverge, so the provider declares whether this fallback is allowed. |
| `message.search` | `message.search` or `query.raw` | Local FTS over the cached mail. The UI says "local results". |
| thread grouping | `thread.native` | Host threader from `Message-ID` / `In-Reply-To` / `References`, then subject. |
| `message.move` | `message.move` | Emulate with `message.copy` + `message.delete` only when both are native and `message.move.emulate` is declared. Never a local-only move. |
| `message.delete` | `message.delete` | No fallback for a remote account. Local-only accounts (imported mail) may remove the row. |
| `message.append` | `message.append` | Unsupported. A Sent copy is a warning after a successful send, not a send failure. |
| `attachment.fetch` | `attachment.fetch` | Refetch the whole message and take the part. |
| `mailbox.create/rename/delete` | matching capability | Unsupported. |
| `send.submit` | `send.submit` | Unsupported. Never pretend a mail was sent. |
| `mailbox.list` / `message.fetch` | core | Last cached list; local cache then provider. |

An operation is **not** allowed to apply the local half of a destructive
native operation when the native half failed. That is the rule that keeps a
failed delete from vanishing from Kimo while remaining on the server.

### 4.1 Server-visible state is remote-first

For `message.set_flags`, `message.set_labels` (provider labels),
`message.move`, `message.delete`, `message.append` and `send.submit`:

1. send the native operation to the provider;
2. wait for a definitive answer;
3. only then apply the returned local effects and emit the UI event.

If the provider fails, the local state is unchanged and the error is shown.
If the provider succeeds but the local transaction fails, the remote change is
**not retried**; the host marks the mailbox dirty, emits a warning, and the
next sync reconciles. Retrying a successful remote delete because SQLite was
busy is how mail gets deleted twice or not at all.

Local-only state — local labels, local flags on POP3, local thread grouping,
local search — is applied directly by the host and never waits for a provider.

### 4.2 Three implementations of the same action

`message.set_labels("Receipts")`:

| Account | What the host does |
| --- | --- |
| Generic IMAP | Label origin `Local`; writes `labels` / `message_labels` only. No IMAP command. |
| Gmail | Provider label; sends `UID STORE +X-GM-LABELS ("Receipts")`; on success mirrors the label and emits `MessageChanged`. |
| Graph | Provider category; sends `PATCH /me/messages/{id}` with the new `categories` array; on success mirrors it. |
| Provider disabled / no native labels | Local label, or `unsupported` if the label's origin is `Provider`. |

`message.delete`:

| Account | What the host does |
| --- | --- |
| IMAP | `UID STORE +FLAGS.SILENT (\Deleted)`, then `UID EXPUNGE` (UIDPLUS) or `EXPUNGE`; on success remove the local row. |
| POP3 | `DELE`, then `QUIT`; only a successful `QUIT` commits the deletion. |
| Gmail | Move to `[Gmail]/Trash` (soft) or expunge the Trash copy (permanent, if declared). |
| Graph | `DELETE /me/messages/{id}` (Deleted Items); permanent delete only if the capability is declared. |
| Local-only | Remove the local row. |

### 4.3 Effects, events and idempotency

- `Operation.id` is minted by the host and is unique per attempt. Providers
  MAY use it as a client token where the remote API supports one.
- Effects are applied once. The host does not re-apply an old result.
- The host emits events **after** the transaction commits: mailbox changed,
  message added/changed/removed, auth required, submission completed, warning.
- The provider does not push events. There is no callback after `invoke`
  returns. A future `watch` capability can add long-lived subscriptions, but
  the first ABI stays request/response.
- If a send's connection is lost after the SMTP `DATA` terminator, the result
  is `ambiguous`, not `failed`. The host MUST NOT retry, MUST NOT claim
  delivery, and MUST NOT create a Sent copy.

### 4.4 Threading, cancellation and deadlines

- Every `invoke` is synchronous and runs on a Kimo `sync::Job` worker (or the
  dedicated extension worker). Nothing in this specification runs on the UI
  thread.
- One provider instance handles one call at a time; the host serialises calls
  for that account. Different accounts may run concurrently.
- A deadline is enforced by the WASM engine: the host meters fuel or uses an
  interrupt handle, and traps a module that runs past its budget. A provider
  still uses the host's socket timeouts and checks `cancelled` between steps,
  so a normal timeout is reported as a timeout rather than a generic trap.
- A WASM module is single-threaded. It cannot spawn threads, and no work may
  call back into the host after `kimo_invoke` returns.
- The host drops the instance after a trap and marks the account unhealthy
  after repeated timeouts; later calls get a structured error, not a hung UI.

---

## 5. The operation catalogue

### 5.1 Mailbox operations

| Operation | Meaning | Native requirement |
| --- | --- | --- |
| `mailbox.list` | Enumerate mailboxes, or the changes since a cursor. | core |
| `mailbox.status` | Counts and sync marks for one mailbox. | `mailbox.status` |
| `mailbox.create` | Create a mailbox. | `mailbox.create` |
| `mailbox.rename` | Rename a mailbox. | `mailbox.rename` |
| `mailbox.delete` | Delete a mailbox; the host confirms and never assumes its mail is gone. | `mailbox.delete` |
| `mailbox.subscribe` | Subscribe to an IMAP mailbox. | `mailbox.subscribe` |
| `mailbox.unsubscribe` | Unsubscribe from an IMAP mailbox. | `mailbox.unsubscribe` |
| `mailbox.expunge` | Expunge messages already marked deleted. | `mailbox.expunge` |
| `mailbox.sync` | Return a page of changes for one mailbox. | core |

### 5.2 Message operations

| Operation | Meaning | Native requirement |
| --- | --- | --- |
| `message.list` | Summaries, paged by mailbox, thread or query. | core |
| `message.fetch` | Body, parts or raw source for a set of identities. | core |
| `message.search` | Structured server search. | `message.search` / `query.raw` |
| `message.thread` | Resolve thread membership. | `thread.native` |
| `message.set_flags` | Add/remove flags. | `message.set_flags` |
| `message.set_labels` | Add/remove native labels. | `message.set_labels` |
| `message.move` | Move between mailboxes. | `message.move` |
| `message.copy` | Copy between mailboxes. | `message.copy` |
| `message.delete` | Trash or permanently delete. | `message.delete` |
| `message.archive` | Move to the archive role. | `message.move` or `message.archive` |
| `message.junk` | Mark junk or move to junk. | `message.junk` |
| `message.append` | Append a raw message to a mailbox (Sent copy, import). | `message.append` |
| `attachment.fetch` | Fetch one attachment's bytes. | `attachment.fetch` |

### 5.3 Submission

| Operation | Meaning | Native requirement |
| --- | --- | --- |
| `send.submit` | Submit a fully built message. | `send.submit` |

`send.save_copy` is a capability, not a separate operation: the host checks it
after `send.submit` and either lets the provider file the Sent copy or calls
`message.append` on the mailbox provider.

`send.submit` receives both a structured message and, for SMTP, the rendered
RFC 5322 bytes:

```rust
struct SubmissionRequest {
    envelope: Envelope,                // from, to, cc, bcc
    message: OutgoingMessage,          // subject, text, html, attachments
    rfc822: Option<BlobRef>,           // required by SMTP; optional for Graph
    save_copy: bool,
    options: SubmissionOptions,        // DSN, SMTPUTF8, ...
}

struct SubmissionResult {
    status: SubmissionStatus,          // Accepted | Rejected | Ambiguous
    accepted: Vec<Address>,
    rejected: Vec<RecipientFailure>,
    response: Option<String>,          // server's own words, sanitised
    remote_id: Option<String>,
}
```

SMTP uses `rfc822`; Graph uses `message` because its `sendMail` action takes a
message object, not a MIME blob. The host renders the MIME once and hands the
same bytes to the Sent copy when the copy is an IMAP `APPEND`.

A message with some recipients accepted and some rejected is `Accepted` with
the rejected addresses in `rejected`: the mail went to the accepted ones. A
final 5xx for every recipient is `Rejected`. A lost connection after the SMTP
`DATA` terminator is `Ambiguous`, never `Rejected`.

A submission provider that declares `send.save_copy` files its own Sent copy.
If it does not, the host calls `message.append` on the mailbox provider after
the submission is `Accepted`. A failed copy is a warning on a sent mail, never
a failed send, and it is not attempted after an `Ambiguous` result.

### 5.4 Authentication

| Operation | Meaning |
| --- | --- |
| `auth.describe` | The methods and fields this provider needs. |
| `auth.begin` | Start a flow; return the first challenge. |
| `auth.continue` | Answer a challenge; return the next challenge or credentials. |
| `auth.cancel` | Abandon the flow and discard partial state. |
| `auth.refresh` | Refresh an OAuth2 token; returns updated credentials. |

```rust
struct AuthChallenge {
    kind: AuthChallengeKind,           // Password, OpenUrl, EnterCode, Poll, Done
    url: Option<String>,
    code: Option<String>,              // user code for device flow
    fields: Vec<ConfigField>,
    message: Option<String>,
    expires_at: Option<Timestamp>,
}
```

The host renders the challenge, opens URLs in the user's browser (never an
embedded web view), collects secrets and stores the returned credentials. The
provider never sees an OAuth refresh token it did not need; it can request
`auth.refresh` through the host when a token expires.

A flow that redirects to a loopback gets its port from the host: the socket
is bound before `auth.begin` runs, and its port arrives in the call's
`params.redirect_port`, so the redirect URI the provider builds names a port
something is listening on. The redirect's `code` and `state` go back in
`auth.continue`'s params; the state is what ties the answer to this window,
and a provider consumes its pending flow with the call, win or lose.

The tokens ride the response envelope's `credentials` field, beside `result`:
`auth.continue`'s `result` is only `{"kind":"done"}`. A host that looks for them
inside `result` reports "the plugin returned no tokens" for a sign-in the
provider did complete.

Where the deployment's OAuth client has a secret, it travels beside the id in
`auth.begin`'s params as `client_secret` and stays inside the module. Google
issues one to desktop clients and its token endpoint refuses the exchange
without it; an Entra public client has none, so a host must offer the field as
optional rather than demand it. An installed app's secret is public by
construction — it ships in the binary — so the spec does not class it with the
credentials the host must protect.

### 5.5 Provider operations

| Operation | Meaning |
| --- | --- |
| `provider.describe` | Static descriptor, config schema, auth methods, limits. |
| `provider.health` | Connect if needed, authenticate, return effective capabilities and warnings. |
| `provider.migrate_config` | Convert a stored config from an older provider version. Optional. |

---

## 6. Standard protocol mappings

### 6.1 IMAP

The IMAP provider owns the IMAP state machine, the modified UTF-7 mailbox
names and the UID arithmetic. It does not own the socket or TLS: the host
provides a TLS stream over the narrow `net.tls` import. The host stores the
wire-form mailbox id and never re-encodes it.

| Operation | IMAP |
| --- | --- |
| `mailbox.list` | `LIST "" "*"` (or a hierarchy walk); parse attributes; `SPECIAL-USE` when advertised. |
| `mailbox.status` | `STATUS mailbox (MESSAGES UNSEEN UIDNEXT UIDVALIDITY)`. |
| `mailbox.create` | `CREATE`; `SUBSCRIBE` if the account subscribes new mailboxes. |
| `mailbox.rename` | `RENAME`. |
| `mailbox.delete` | `DELETE`; optionally `UNSUBSCRIBE` first. |
| `mailbox.subscribe` | `SUBSCRIBE` / `UNSUBSCRIBE`. |
| `mailbox.expunge` | `EXPUNGE`, or `UID EXPUNGE` with UIDPLUS. |
| `mailbox.sync` | `SELECT`; `UID FETCH <cursor>:* (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE BODYSTRUCTURE)`; `UIDNEXT`/UIDVALIDITY from `SELECT`. |
| `message.list` | `UID FETCH` over the requested range with the same summary items. |
| `message.fetch` | `UID FETCH <uid> (BODY.PEEK[])`; `BODY.PEEK[HEADER]` or `BODY.PEEK[TEXT]` for a part. |
| `message.search` | `UID SEARCH` with a built query; `CHARSET UTF-8` only when advertised. |
| `message.thread` | `THREAD REFERENCES` when the `THREAD` extension exists; otherwise local. |
| `message.set_flags` | `UID STORE <uid> +FLAGS.SILENT (...)` / `-FLAGS.SILENT (...)`. |
| `message.set_labels` | Not native; local labels. |
| `message.move` | `UID MOVE` with `MOVE`; otherwise `UID COPY` + `\Deleted` + `UID EXPUNGE` (UIDPLUS). |
| `message.copy` | `UID COPY`. |
| `message.delete` | `UID STORE +FLAGS.SILENT (\Deleted)`, then `UID EXPUNGE`; without UIDPLUS a mailbox-wide `EXPUNGE` is a declared, visible limitation. |
| `message.append` | `APPEND mailbox (flags) date literal`. |
| `attachment.fetch` | `UID FETCH` of the relevant `BODY.PEEK[part]`, or a full fetch and parse. |
| `query.raw` | Optional dialect `imap`; not advertised by default. |

Traps the provider MUST keep:

- `BODY.PEEK` is the default for reading. A fetch that marks `\Seen` is a
  separate, explicit operation.
- A UID is meaningless without its UIDVALIDITY. A generation change resets the
  mailbox; it does not merge old UIDs into the new generation.
- `UID EXPUNGE` is not available without UIDPLUS. Falling back to `EXPUNGE`
  removes every `\Deleted` message in the mailbox, not only the user's
  selection; the host must say so before using it.
- `UID MOVE` is not `COPY` + `DELETE`; if MOVE is unavailable the emulation
  is a capability with its own warning.
- Mailbox names stay in wire form. Display decoding happens only at the edge.

### 6.2 POP3

POP3 is a small `kimo-pop3/1` provider. It has one mailbox, no server-side
flags, no labels, no move and no search.

| Operation | POP3 |
| --- | --- |
| `mailbox.list` | One `INBOX`; `STAT` for counts. |
| `mailbox.sync` | `UIDL`; compare with known UIDLs; `TOP n 0` for headers; `RETR n` for new bodies. |
| `message.list` | `LIST` / `UIDL` plus `TOP` headers. |
| `message.fetch` | `RETR`; de-stuff dot-lines. |
| `message.delete` | `DELE` then `QUIT`; only `QUIT` commits. |
| `message.set_flags` | Local only (seen/flagged). |
| `message.set_labels` | Local only. |
| `message.search` | Local only. |
| `message.thread` | Local only. |
| `message.move/copy` | Unsupported. |
| `message.append` | Unsupported. |
| `attachment.fetch` | Refetch the message and parse the part. |
| `mailbox.create/rename/delete` | Unsupported. |
| `send.submit` | Separate `kimo-smtp/1` provider. |

POP3's `DELE` is a session state, not a durable operation. If the connection
drops before `QUIT`, the server may restore the message. The host must not
remove the local row until `QUIT` is acknowledged. POP3 does not report
deletions made by another client; the provider MUST declare `sync.deletions`
as absent.

### 6.3 SMTP

The SMTP provider is a `kimo-smtp/1` submission provider. It has no mailbox
operations, and it speaks SMTP over a host-provided TLS stream rather than
opening a socket itself.

| Stage | Command / behaviour |
| --- | --- |
| greeting | Read the multiline banner. |
| capabilities | `EHLO`; parse `SIZE`, `8BITMIME`, `SMTPUTF8`, `PIPELINING`, `DSN`, `AUTH`. |
| TLS | Implicit TLS on 465, or `STARTTLS` on 587/25; never send credentials before TLS. |
| auth | `AUTH PLAIN`, `AUTH LOGIN` (legacy), `AUTH XOAUTH2`, `AUTH OAUTHBEARER` where advertised. |
| envelope | `MAIL FROM:<...>`, `RCPT TO:<...>` per recipient. |
| body | `DATA`, dot-stuff, `.`; `BDAT` only if declared. |
| result | 2xx after the terminating dot means accepted; 4xx/5xx means refused. |
| quit | `QUIT`; a failed `QUIT` does not undo an accepted message. |

The provider MUST NOT retry after the terminating dot if the connection was
lost. The result is `Ambiguous`. Before `DATA`, a retry is safe and may be
attempted once, but the host still owns retry policy; the provider does not
loop forever.

If `SIZE` is advertised and the rendered message exceeds it, fail before
`MAIL FROM`. If `SMTPUTF8` is not advertised, non-ASCII addresses are a
failure, not a silent downgrade. If `8BITMIME` is absent, the message must be
encoded to 7 bits.

### 6.4 Gmail `X-GM-EXT-1`

Provider id `gmail`. It uses IMAP for mail and SMTP for submission, and
advertises the Gmail profile only when the server returns `X-GM-EXT-1` in its
capability list. If the extension is absent it degrades to `kimo-imap/1`:
local labels, local threads, no raw query.

**Authentication.** OAuth2 with the `https://mail.google.com/` scope (or a
narrower scope the provider can prove sufficient). IMAP `AUTHENTICATE
XOAUTH2`; SMTP `AUTH XOAUTH2`. App passwords are a separate
`auth.password` method, not a fallback that silently appears.

**Mapping.**

| Kimo | Gmail |
| --- | --- |
| mailbox | Gmail label, including system labels. |
| `identity.global` | `X-GM-MSGID` (decimal 64-bit, stored as a string). |
| `thread.id` | `X-GM-THRID` (decimal 64-bit, stored as a string). |
| native labels | `X-GM-LABELS` on `FETCH`; `UID STORE +X-GM-LABELS` / `-X-GM-LABELS`. |
| `query.raw` dialect `gmail` | `UID SEARCH X-GM-RAW "..."`. |
| `message.delete` soft | Add `\Trash`, remove `\Inbox` where appropriate. |
| `message.delete` permanent | Expunge the Trash copy, if the server allows it; otherwise declare the capability absent. |
| `send.submit` | Gmail SMTP with XOAUTH2, or the provider's own submission path. |

Gmail's labels are not folders. Moving a message between two labels is a
membership change, not a copy. The host must keep one message with several
locations. Removing the last location removes the local message; removing one
label must not delete the mail from the other labels.

System labels (`\Inbox`, `\Sent`, `\Drafts`, `\Spam`, `\Trash`,
`\All`, `\Starred`, `\Important`) are mirrored as provider labels with their
special roles. User labels can contain `/`; the provider handles the wire
encoding and the host stores Unicode names.

**Fallback.** If a Gmail account is configured but the Gmail plugin is
disabled, the account is unavailable as a Gmail account. Do not silently
switch it to generic IMAP: the user would lose labels, native threads and raw
query without being told. A generic IMAP account remains available as a
separate account choice.

### 6.5 Microsoft Graph (Outlook / Microsoft 365)

Provider id `outlook`. It uses Graph v1.0 over HTTPS through the host's
`net.http` import; it does not use IMAP or SMTP. It can be disabled by the
user, in which case its accounts are unavailable until it is enabled again.

**Authentication.** OAuth2 authorization code with PKCE, or device code for a
desktop without a loopback listener. Public client, no client secret. Scopes:
`offline_access`, `Mail.ReadWrite`, `Mail.Send` (plus `.Shared` for shared
mailboxes when the provider supports them). The host opens the system browser
and receives the redirect; no embedded web view.

**IDs.** Every request sends `Prefer: IdType="ImmutableId"`. With immutable
IDs, a message keeps its id when moved. If a server or tenant does not honour
the header, the provider falls back to `internetMessageId` plus folder and
declares the weaker identity in its descriptor.

**Mapping.**

| Kimo | Graph |
| --- | --- |
| mailbox | `/me/mailFolders`; `wellKnownName` maps inbox, drafts, sentitems, deleteditems, archive, junkemail; custom folders otherwise. |
| `mailbox.list` | `GET /me/mailFolders?$select=id,displayName,parentFolderId,wellKnownName,totalItemCount,unreadItemCount`; walk `childFolders`. |
| `mailbox.create` | `POST /me/mailFolders`. |
| `mailbox.rename` | `PATCH /me/mailFolders/{id}` with `displayName`. |
| `mailbox.delete` | `DELETE /me/mailFolders/{id}`. |
| `mailbox.sync` | `GET /me/mailFolders/{id}/messages/delta`; persist `@odata.deltaLink`; follow `@odata.nextLink`. |
| `message.list` | `GET /me/mailFolders/{id}/messages` with `$select` and `$top`; or `$search` / `$filter`. |
| `message.fetch` | `GET /me/messages/{id}` with `$select=body,uniqueBody,internetMessageHeaders,...`; or `$expand=attachments`. |
| `message.set_flags` | `PATCH /me/messages/{id}` with `isRead`, `flag`, `importance`. |
| `message.set_labels` | `PATCH /me/messages/{id}` with `categories`. |
| `message.move` | `POST /me/messages/{id}/move` with `destinationId`. |
| `message.copy` | `POST /me/messages/{id}/copy`. |
| `message.delete` soft | `DELETE /me/messages/{id}`; Graph moves it to Deleted Items. |
| `message.delete` permanent | Only if the tenant exposes a permanent-delete action; otherwise declare the capability absent. |
| `message.append` | Create a draft in Drafts; Sent filing is native to `sendMail` with `saveToSentItems`. |
| `message.search` | `$search` (KQL) or `$filter`; the provider builds it, the host does not. |
| `query.raw` dialect `graph-kql` | `$search="..."`; the provider escapes it and reports Graph's combination limits. |
| thread | `conversationId`. |
| `send.submit` | `POST /me/sendMail` with `saveToSentItems`; or create a draft and `POST /me/messages/{id}/send`. |
| attachments | `contentBytes` for small files; upload session for large files. |
| `attachment.fetch` | `GET /me/messages/{id}/attachments/{id}`; `contentBytes` or raw. |

**Graph traps.**

- Graph's `sendMail` request has a size limit (currently 4 MB). A larger
  message is built as a draft, attachments go through an upload session, and
  the draft is sent. The provider declares whether this path is available.
- `$search` and `$filter` have different, tenant-dependent limits and cannot
  always be combined with ordering or paging. The provider post-filters where
  necessary and reports `query.raw` as a provider dialect, not as SQL.
- A delta token can expire or be invalidated. The provider returns
  `reset: true`; the host clears the mailbox's locations and refetches.
- 429 responses carry `Retry-After`; map them to `rate_limited` with
  `retry_after_ms`. Do not hammer.
- Categories are flat strings; Gmail's nested labels and Graph's categories
  are not interchangeable. Do not silently rewrite one into the other.
- Graph's `DELETE` is a soft delete. A "permanent delete" that actually
  bypasses Deleted Items is a separate, declared capability.

---

## 7. The local store contract

The host owns the database described in [storage.md](storage.md). This draft
extends the shape; a migration must preserve the existing cached mail.

### 7.1 Proposed shape

```text
accounts        id, provider, provider_config, provider_config_version,
                submission_provider, submission_config, enabled, ...
mailboxes       id, account_id, remote_id, name, parent_id, role,
                attributes, generation, cursor
messages        id, account_id, global_remote_id, subject, from, to, cc,
                date, flags, snippet, ..., fts columns
message_locations  message_id, mailbox_id, remote_id, generation
labels          id, account_id, name, origin, remote_id, color, parent_id
message_labels  message_id, label_id
threads         id, account_id, remote_id, origin, subject, latest
thread_members  message_id, thread_id
```

- `messages` is keyed by the host's local id, not by `(folder, uid)`.
  `message_locations` carries the provider's location keys. This is what lets
  a Gmail message exist under `INBOX` and `Receipts` without two copies.
- `generation` is IMAP UIDVALIDITY, a Graph delta generation, or NULL. A new
  generation resets the mailbox locations; it never merges UIDs across
  generations.
- `labels.origin` is `Local` or `Provider`. A provider label carries its
  remote id; a local label never does.
- `folders` in the current schema becomes `mailboxes`; `uid_next`,
  `first_num` and `oldest_uid` move into the provider-defined cursor blob,
  not a protocol assumption.
- The FTS5 index stays on `messages`; local search is unchanged from
  [the search section of storage.md](storage.md#search).

### 7.2 No durable operation queue

There is no outbox, no retry queue and no pending-operation table in this
draft. A failed operation returns an error and the UI keeps the user's intent
on screen where it can be retried by a click. This is consistent with hard
rule 4. The one exception is a submission result of `Ambiguous`: the host
records a warning for the session, but it does not queue or resend.

### 7.3 Provider changes

Changing an account's provider (for example Gmail to generic IMAP) changes the
meaning of remote ids and native labels. The host MUST treat it as a new
mailbox identity: cached remote mail for the old provider is purged or marked
stale, and local labels are preserved only if they can be attached to messages
by a stable identity. A provider change is never a silent relabel.

### 7.4 Migration from today's database

- `accounts.protocol = imap|pop3` becomes `provider = imap|pop3`.
- The five `smtp_*` columns become a `submission` record with provider
  `smtp`.
- Existing `folders` become `mailboxes`; `messages` rows gain a location in
  their folder. UIDVALIDITY is preserved as `generation`.
- Existing local labels keep `origin = Local`; no label is guessed to be
  native.
- Threads are rebuilt by the current local threader; no native thread ids
  exist yet for old mail.
- No mail is deleted by the migration. If a mapping is ambiguous, the row
  keeps a legacy location and is marked for a resync rather than dropped.

---


## 8. The plugin ABI

### 8.1 Manifest and discovery

A plugin is a directory with a `plugin.json` manifest and the WebAssembly
module it names:

```json
{
  "schema": 1,
  "id": "org.kimo.outlook",
  "name": "Outlook / Microsoft 365",
  "version": "0.1.0",
  "abi": 1,
  "module": "outlook.wasm",
  "default_enabled": false,
  "permissions": ["net.http", "secrets"],
  "providers": [{ "id": "outlook", "name": "Microsoft 365 / Outlook" }],
  "extensions": []
}
```

- User plugins: `$XDG_DATA_HOME/kimo/plugins/<dir>/` (normally
  `~/.local/share/kimo/plugins/`).
- System plugins: `/usr/lib/kimo/plugins/<dir>/` (packaging).
- Development: `KIMO_PLUGIN_PATH`, a colon-separated list, highest priority.
- Discovery order is env, user, system. A duplicate plugin id keeps the first
  one and logs the shadowed path; it does not load both.
- `module` is relative to the plugin directory, must not contain `..`, must
  end in `.wasm`, and must exist. Symlinks are resolved before reading.
- The host validates the module before running it: the WebAssembly magic, a
  matching `abi`, a `kimo_abi_version` export returning 1, and an import set
  covered by the manifest's permissions. There is no `dlopen`, no
  `RTLD_NOW`/`RTLD_LOCAL` and no `dlclose`; a `.so`, `.dylib` or `.dll` is
  never loaded.
- A missing, invalid or rejected module is reported in Preferences → Plugins;
  it does not stop Kimo from starting.

A future workspace separates the boundary, the engine and the modules:

```text
kimo/
  crates/kimo-plugin-abi/       JSON types + WASM ABI; no host dependency
  crates/kimo-plugin-host/      engine, permissions, discovery, dispatch
  plugins/imap/                 wasm32-unknown-unknown, kimo-imap/1
  plugins/pop3/                 wasm32-unknown-unknown, kimo-pop3/1
  plugins/smtp/                 wasm32-unknown-unknown, kimo-smtp/1
  plugins/gmail/                wasm32-unknown-unknown, kimo-gmail/1
  plugins/outlook/              wasm32-unknown-unknown, kimo-graph/1
  extensions/                   wasm32-unknown-unknown, organizer.v1 / display.v1
```

Plugins are built for `wasm32-unknown-unknown` and stored as `.wasm`. Build
them with that target; the ABI crate supplies the allocator and the host-import
declarations. They do not import WASI in v1 and never link the Kimo binary, a
platform TLS stack, or a C library. A future WASI reactor is possible only if
the host exposes a WASI surface explicitly; the default engine configuration
exposes none.

The ABI crate is the only shared dependency between the host and a plugin.
Provider crates may use extracted pure-Rust protocol crates, but they never
link the host. The host links the ABI crate and the engine, not the provider
crates.

The reference providers (`imap`, `pop3`, `smtp`) are shipped as `.wasm`
plugins too. Packaging must install them; a missing reference module leaves its
accounts unavailable with an explicit error. There is no hidden statically
linked fallback in v1.

### 8.2 The WebAssembly boundary

Every module exports the same small surface. There is no C ABI, no Rust trait
object and no function pointer crossing a shared-library boundary:

```text
kimo_abi_version() -> u32
kimo_alloc(len: u32) -> u32
kimo_free(ptr: u32, len: u32)
kimo_invoke(request_ptr: u32, request_len: u32) -> u64
memory
```

`kimo_invoke` receives UTF-8 JSON and returns a packed result: the high 32
bits are the result pointer, the low 32 bits are the result length. A zero
pointer/length is an empty response; a structured error is still JSON. The
host calls `kimo_alloc` for the request, copies the JSON into the module's
linear memory, calls `kimo_invoke`, copies the result out, then calls
`kimo_free` for both buffers. The module's allocator owns its own memory; the
host never keeps a pointer after the call.

The host imports are deliberately narrow; the network imports and credential
delivery are gated by the manifest:

| Import | Permission | Purpose |
| --- | --- | --- |
| `kimo.log` | none | A redacted log line. |
| `kimo.now_ms` | none | Wall-clock time for deadlines and token metadata. |
| `kimo.cancelled` | none | Poll between long steps. |
| `kimo.random` | none | Nonces; the host uses its CSPRNG. |
| `kimo.http_request` | `net.http` | Graph and OAuth2 token endpoints. |
| `kimo.net_open`, `kimo.net_starttls`, `kimo.net_read`, `kimo.net_write`, `kimo.net_close` | `net.tls` | IMAP, POP3 and SMTP over a host-owned stream. |
| `kimo.blob_read`, `kimo.blob_write`, `kimo.blob_free` | none | Large bodies and attachments. |

- The import allowlist is the `kimo` namespace only; a WASI or `env` import
  is rejected in v1. The host never hands the module a database handle, an
  egui context, a file descriptor, an environment variable or a process
  handle. There is no import for filesystem, environment, subprocess, threads
  or raw OS sockets.
- Network calls take endpoints from the provider descriptor or the account
  configuration; the host checks the target against the declared network
  rules and re-checks redirects. A plugin cannot open an arbitrary host merely
  because it has `net.tls`.
- The host owns DNS, TCP, TLS, HTTP, redirect policy, proxies, socket
  timeouts, response-size caps and certificate verification. A plugin cannot
  disable certificate verification or reach plaintext outside a declared
  STARTTLS path.
- `kimo.http_request` answers an *envelope*, `{"status", "headers", "body"}`,
  with the endpoint's bytes base64'd into `body` — never the endpoint's JSON
  itself. A module decodes `body` (`abi::decoded_body`) and only then parses
  it; reading `access_token` off the envelope compiles, and its only symptom is
  "the token endpoint answered with nothing usable".
- Every data-bearing host call takes pointers into the module's linear memory
  and returns a status or byte count. The host copies data in and out before
  returning; no host pointer survives a call.
- The host instantiates the module once per contribution (a provider account,
  or an extension id). Instances are single-threaded and have separate
  memories. A provider cannot see an extension's memory, and one account
  cannot see another's.

Kimo runs the module in a pure-Rust WebAssembly interpreter (for example
`wasmi`), not a JIT and not a native shared library. The interpreter gives the
host validation before execution, memory isolation, deterministic fuel
metering and a trap for a runaway module. SQLite remains the only C in the
tree; a `.wasm` module is data, not a C dependency.

### 8.3 Traps, memory and ownership

- A Rust panic becomes a WASM trap. The host maps it to a structured
  `plugin_trapped` error and drops the instance; it cannot abort the Kimo
  process the way an unwinding native shared library can.
- The plugin need not catch panics. It SHOULD return a structured error for
  expected failures before it reaches a trap; a trap is for a bug.
- The host and the module never free each other's memory. `kimo_alloc` and
  `kimo_free` are the only allocator boundary; dropping the instance releases
  everything it owns.
- No Rust type with a destructor crosses the boundary. JSON is copied in and
  out; the result buffer lives until the host calls `kimo_free`.
- A plugin MUST NOT call into the host after `kimo_invoke` returns. There are
  no host callbacks, no threads and no work that outlives the call.
- A module that exceeds its memory cap or fuel budget traps. The host reports
  `too_large` or `timeout` and does not retry the same call blindly.

### 8.4 Versioning and compatibility

- `abi` is a major version. The host refuses a module whose
  `kimo_abi_version` or manifest `abi` names another major.
- Within ABI 1, new optional JSON fields and new capability names are
  additive. Unknown fields are ignored; unknown effect variants or operation
  names are rejected.
- The export set and the import set are part of the ABI. Adding a host import
  is additive only for a module that also declares the matching permission and
  minimum host version; an old host refuses it at validation time.
- `spec_version` is the document version (`1.0`). A plugin declares the
  minimum host spec it needs.
- A capability is never quietly redefined. Removing or renaming one is a new
  major profile/ABI.
- Provider configs carry a version. `provider.migrate_config` is optional; a
  provider that cannot migrate a config refuses the account rather than
  guessing.
- A `.wasm` built for a newer ABI is rejected whole; it is never partially
  instantiated.

### 8.5 Payloads and blobs

Small metadata and operation results travel as JSON. Mail bodies and
attachments can be large, so the ABI reserves host-owned **blob handles**:
JSON carries `{"blob": "<id>"}`, and the module reads or writes bytes through
`blob_read` / `blob_write` with an explicit offset and length. This avoids
base64-inflating a 20 MB attachment and keeps the bytes out of the JSON
envelope.

For ABI 1, a host MAY inline payloads up to 8 MiB as base64 and MUST support
blob handles for anything larger. A module that only handles inline payloads
declares a limit; the host refuses larger messages instead of truncating them.
Blob handles are bound to the instance and the call; the host releases them
when the call ends. A plugin never reads a file from disk to obtain one.

### 8.6 Execution, limits and health

- The host calls a provider only from a Kimo `sync::Job` worker; the UI thread
  never enters `kimo_invoke`.
- One instance handles one call at a time. The host serialises calls for an
  account; different accounts may run concurrently.
- Extension hooks get their own worker and their own instance; a slow provider
  cannot block a display hook.
- Limits: a module's memory has a hard cap (64 MiB for an extension, 128 MiB
  for a provider by default), and each call has a fuel/instruction budget and
  a wall-clock deadline. JSON request and response are capped at 32 MiB each
  (excluding blobs), an effect list at 10,000 entries, and a page at 1,000
  mailboxes or messages.
- A provider operation must fit its call's budget. `mailbox.sync` returns a
  page; `more: true` says the module stopped before the range was done, and
  the host calls again with the mark the page stored. Fetching a whole
  folder's bodies in one call exhausts the fuel long before the mailbox is
  synced — the Gmail module pages at 4 MiB for exactly this reason.
- The host traps a module that runs past its fuel or deadline. A provider that
  traps or times out repeatedly is marked unhealthy for the session and its
  instance is dropped.
- A module error is shown on the account or in Preferences → Plugins. It is
  never allowed to take down the process.
- Modules are deterministic apart from the host functions they call. There is
  no ambient clock, entropy or thread scheduling inside the sandbox.

### 8.7 Trust, capabilities and secrets

A WASM module has no ambient authority. It cannot open a socket, read a file,
spawn a process, read an environment variable or touch the database unless the
host exposes an import for it, and the manifest declares the permission.

- The enforced permission set is `net.http`, `net.tls` and `secrets`. A
  module that imports `http_request` without `net.http`, or
  `net_open`/`net_starttls` without `net.tls`, is rejected before
  instantiation. Credentials are included in the request only when
  `secrets` is granted.
- The UI shows the module, its id, its version, its ABI and its permissions.
  A provider can still read the mail it is given and send it to the endpoints
  it is allowed; the sandbox prevents ambient access, not a malicious provider
  using its granted network. Install providers you trust.
- Extension instances get no network and no secrets: the host links the
  network imports to stubs that return `denied`, and omits credentials from an
  extension request even when the same module also contributes a provider.
  They receive summaries or the parsed body the host chooses to pass, and
  return a declarative plan.
- Kimo never downloads or auto-updates a module. The module is a local file
  the user installed; a checksum in a manifest is not a signature.
- TLS is host-owned. A plugin cannot disable certificate verification, set a
  proxy, or reach plaintext outside a declared STARTTLS path.
- A plugin that handles HTML mail does not bypass the host's sanitiser or
  remote-resource policy. Extensions can request stricter behaviour, never
  looser.
- Credentials are passed only to a provider with `secrets`, for the length
  of the call. They are redacted from `Debug`, logs and error strings.

### 8.8 Enable, disable and failure

- `default_enabled` is set by the manifest: `imap`, `pop3` and `smtp`
  default on; `gmail` may default on; `outlook` defaults off until the user
  enables it.
- Plugin instances are disposable; there are no native function pointers to
  keep alive. A provider toggle still takes effect at the next start, because
  an in-flight account job must not change provider underneath the user. An
  extension toggle takes effect at the next hook request.
- Disabling a mailbox provider leaves its cached mail readable. Remote actions
  and submission are disabled with a banner. Re-enabling restores them.
- Disabling an extension falls back to the host's built-in organization and
  display immediately (at the next hook request).
- A module that fails validation, compilation or instantiation is not retried
  in a loop. It appears in the Plugins panel with the engine's error.
- Replacing a module on disk takes effect at the next start, when a new
  instance is created and validated.

## 9. Extension hooks

Extensions are Rust `.wasm` plugins with the same ABI, but their contributions
are GUI hooks rather than providers. They return **declarative plans**; they do
not receive an egui context, a renderer, or a database handle.

### 9.1 What an extension can hook

| Hook | Purpose | Version |
| --- | --- | --- |
| `organizer.v1` | Group, order and annotate a list of messages (threads, sender, labels, date, custom groups). | 1 |
| `display.v1` | Add or replace the reading pane's head sections, badges, actions, and choose/transform the content plan. | 1 |

Reserved for later, not part of ABI 1: `compose.v1`, `notify.v1`,
`account.v1`. Reserving a name does not implement it.

An extension is not a provider. It cannot call IMAP or Graph directly through
the host, cannot write the store and cannot send mail. Its action buttons ask
the host to perform a normal operation against the selected messages; the host
validates that operation and shows the usual confirmation for destructive
actions.

### 9.2 Organizer hook

Request:

```json
{
  "hook": "organizer.v1",
  "account": { "id": "work", "provider": "gmail" },
  "mailbox": { "id": "INBOX", "role": "inbox" },
  "messages": [
    {
      "id": "m1",
      "thread": "t1",
      "subject": "Quarterly report",
      "from": [{ "name": "Ada", "address": "ada@example.com" }],
      "date": "2026-10-27T09:00:00Z",
      "labels": ["work"],
      "flags": { "seen": false, "flagged": true }
    }
  ],
  "current": { "threading": true, "sort": "date_desc" },
  "theme": { "scheme": "dark" },
  "limits": { "max_groups": 500, "max_messages": 5000 }
}
```

Response:

```json
{
  "groups": [
    {
      "id": "thread:t1",
      "title": "Quarterly report",
      "subtitle": "Ada · 3 messages",
      "message_ids": ["m1", "m2", "m3"],
      "order": 0,
      "collapsed": false,
      "badges": [{ "text": "3", "tone": "accent" }]
    }
  ],
  "row_overrides": [
    { "message_id": "m1", "badges": [{ "text": "VIP", "tone": "warning" }] }
  ]
}
```

Rules:

- Every input message appears exactly once, in some group or in the default
  ungrouped list. If one is missing or duplicated, the whole plan is rejected
  and the built-in organizer is used.
- `order` is a stable integer; equal orders fall back to the host's date sort.
  A plan must not depend on hash-map iteration order.
- Unknown fields are ignored. Unknown message ids are dropped and reported as
  a warning, not used to hide mail.
- A plan is limited to 500 groups and 5,000 message placements (the values
  in the request); a larger plan fails and falls back.
- `badges[].tone` is a semantic token (`muted`, `accent`, `success`,
  `warning`, `danger`). No raw colours.
- The hook may not read the bodies. It gets summaries only. That keeps the
  list fast and makes the organizer deterministic.

The host calls the organizer when the message list revision, mailbox, sort or
threading preference changes. It caches the last valid plan keyed by that
revision. The UI draws the built-in grouping until the worker returns; a stale
result for an older revision is discarded.

### 9.3 Display hook

Request:

```json
{
  "hook": "display.v1",
  "account": { "id": "work", "provider": "outlook" },
  "message": {
    "id": "m1",
    "thread": "t1",
    "subject": "Quarterly report",
    "from": [{ "name": "Ada", "address": "ada@example.com" }],
    "to": [{ "address": "me@example.com" }],
    "date": "2026-10-27T09:00:00Z",
    "flags": { "seen": true, "flagged": false },
    "labels": ["work"]
  },
  "body": {
    "has_text": true,
    "has_html": true,
    "text": "…",
    "html": "<p>…</p>",
    "attachments": [{ "id": "a1", "name": "report.pdf", "size": 120000 }]
  },
  "host": {
    "sections": ["from", "to", "cc", "date", "subject", "labels", "flags"],
    "content": "auto"
  },
  "theme": { "scheme": "dark" }
}
```

Response:

```json
{
  "head": {
    "sections": [
      {
        "id": "from",
        "op": "replace",
        "rows": [
          { "type": "text", "label": "From", "value": "Ada <ada@example.com>" }
        ]
      },
      {
        "id": "security",
        "op": "append",
        "title": "Security",
        "rows": [
          { "type": "badge", "text": "External sender", "tone": "warning" }
        ]
      }
    ],
    "actions": [
      {
        "label": "Open report",
        "icon": "document",
        "operation": "open_attachment",
        "params": { "attachment": "a1" }
      }
    ]
  },
  "content": {
    "mode": "keep"
  }
}
```

Head rules:

- The row shapes are: `text` (`label`, `value`, `tone`), `badge` (`text`,
  `tone`), `link` (`label`, `href`) and `button` (`label`, `icon`,
  `operation`, `params`).
- Sections have stable ids. `op` is `replace`, `append`, `prepend` or
  `remove`. The host keeps the section order otherwise.
- A row is `text`, `badge`, `link` or `button`. A button names a host
  operation; it cannot name an arbitrary command or URL handler. A link is
  `http`, `https` or `mailto`; the host opens it with the platform's handler.
- An action that sends, deletes or labels is still a user click through the
  host's normal confirmation. An extension cannot make a change just by
  returning a plan.
- The host always draws an account/provider provenance row and any security
  warning it owns; an extension can add to that region but cannot remove the
  host's security notice.
- `tone` is semantic. No CSS, no hex, no font names. The host's theme remains
  the only source of colours.

Content rules:

- The part shapes are: `text` (`text`), `html` (`html`, `remote`) and
  `notice` (`text`, `tone`). A content plan is limited to 500 parts.
- Large bodies are passed as blob refs (section 8.5); the example inlines a
  small one.
- `mode: "keep"` leaves the host's renderer choice alone.
- `mode: "text"` supplies a plain-text alternative; the host may still offer
  the original.
- `mode: "html"` supplies HTML that goes through the host's existing
  `html.rs` / Blitz path and the same sanitiser. No JavaScript, no event
  handlers, no remote-resource override.
- `mode: "parts"` supplies a list of text/HTML/notice blocks; it is for
  extensions that build a reading view from parsed parts, not for arbitrary
  painting.
- `remote` is `host` (use the host setting) or `block` (stricter). An
  extension cannot turn remote images on when the host has them off.
- A plan cannot include a `data:` URL or an inline script. Images are
  attachment references or HTTPS URLs fetched by the host under its policy.

### 9.4 Hook timing, errors and fallback

- Hooks run on a dedicated worker, never in `KimoApp::ui`. The host shows the
  built-in plan until the worker returns.
- A hook call has a soft budget of 50 ms, a hard response limit of 256 KiB and
  a WASM fuel cap. The host can trap a module that exceeds the cap. A hook that
  exceeds the budget is not called again for that revision, and repeated
  failures disable the extension for the session.
- A malformed plan is rejected as a whole. The host logs the reason, uses the
  built-in plan, and shows the extension's error in Preferences → Plugins.
- A trapping or malformed extension is contained by the WASM engine and
  treated as a failed plan.
- The built-in organizer and display are always present, even with every
  extension disabled. Extensions augment; they are not required for a usable
  client.

### 9.5 A minimal extension example

The smallest useful extension is an organizer that groups by sender:

```json
{
  "groups": [
    {
      "id": "sender:ada@example.com",
      "title": "Ada",
      "subtitle": "2 messages",
      "message_ids": ["m1", "m2"],
      "order": 0
    }
  ]
}
```

It reads only summaries, returns a deterministic order, and falls back to the
host's threader if the plan is rejected. A display extension that adds an
"External sender" badge is the same shape: one section, one badge, no drawing.

---

## 10. Kimo UI integration

### 10.1 Preferences → Plugins

- List every discovered plugin: name, id, version, ABI, path, providers,
  extensions, enabled/disabled, last error.
- Enable/disable toggles. A provider change says "takes effect after restart";
  an extension hook applies at the next request.
- No download, no update button, no plugin store. The panel shows the
  module and the permissions it was granted, and says that a provider can
  still read the mail it is given and reach the endpoints it is allowed.
- A failed plugin shows the loader error and the path; it never blocks the rest
  of the list.

### 10.2 Account setup

- The provider picker lists only enabled mailbox providers, plus the generic
  `imap` / `pop3` entries that are enabled.
- The form is generated from `ProviderDescriptor.config`: text, secret,
  number, boolean, select. No provider-specific Rust UI in the host.
- "Test" calls `provider.health`, authenticates if needed and displays the
  effective capabilities and warnings.
- Submission is a second picker: `smtp` (default), the mailbox provider itself
  when it declares `send.submit`, or none.
- An account with no submission provider is readable; its Send is disabled
  with the existing account-level reason.

### 10.3 Disabled provider

Cached mail stays readable. The account shows a banner: provider disabled,
remote actions unavailable, sending disabled. Local labels and local search
still work. The user can re-enable the plugin and restart, or create a generic
IMAP account instead. The host does not silently change the account's provider.

### 10.4 Errors

Provider errors are mapped to the existing `MailError` categories where
possible: `auth_failed` → `MailError::Auth`; `network`/`protocol` →
`MailError::Protocol`; a server refusal → `MailError::Refused`. A plugin
error never exposes a token, a password or a full URL with credentials.
Retryability and `retry_after_ms` are shown when present.

---

## 11. Rollout and tests

### 11.1 Phases

1. **Extract the host boundary.** Introduce the operation/effect types, the
   host-owned store adapter and a test provider. No behaviour change to the
   current UI.
2. **Ship the reference plugins.** Move IMAP, POP3 and SMTP behind the ABI as
   `.wasm` plugins at feature parity. Migrate accounts and cached mail. Keep the
   current tests green.
3. **Gmail.** Add OAuth2/XOAUTH2, X-GM ids, labels, native threads and
   `X-GM-RAW`, with a plain-IMAP degrade path. This is the first real test of
   provider-specific operations and native labels.
4. **Outlook.** Add Graph auth, folders, delta sync, messages, categories,
   threads, search and send. Ship it disabled by default.
5. **Extensions.** Implement `organizer.v1` and `display.v1`, the declarative
   renderer, the worker and fallbacks. Ship a sample organizer and a sample
   head badge extension.
6. **Hardening.** Fuzz the JSON boundary, the extension plans and the effect
   validator; measure fuel budgets; package the reference `.wasm` modules;
   document the capability model.

OAuth2 is a change to today's hard rule 4. It must be an explicit owner
decision before phase 3, not a side effect of "the plugin API made it easy".

### 11.2 Conformance tests

A provider plugin is conforming when:

- its descriptor's capabilities match what it actually does;
- every undeclared operation returns `unsupported`, not a trap or a silent
  local change;
- a generation reset produces a reset effect, not a merge;
- deletion, move and label operations report exactly the effects they made;
- no secret appears in a response, log or error;
- a malformed request or an oversized result is a structured error;
- a module that imports a host function without its declared permission is
  rejected before instantiation;
- a trap or a fuel exhaustion is contained, reported, and does not kill Kimo;
- the same command run twice is idempotent where the remote API allows it;
- a send reports `Ambiguous` after a lost connection following `DATA`;
- an extension plan that drops, duplicates or invents messages is rejected and
  the built-in plan is used.

Test fixtures: a scripted IMAP/POP3/SMTP server, a Gmail X-GM fixture, and a
Graph mock with paging, delta, 429 and expired-token responses. No conformance
test needs a live account.

### 11.3 Acceptance for the first implementation

- Existing IMAP, POP3 and SMTP accounts still work after migration.
- The same generic IMAP account can use local labels, server flags, local
  search and IMAP delete without the UI asking which protocol it is.
- A Gmail account changes a native label and sees it on another client; a raw
  Gmail query returns the same set as Gmail's own search for a fixture.
- An Outlook account can be disabled, then re-enabled, without losing cached
  mail; Graph folders, categories, conversations and send work.
- A sample organizer can group by sender and a sample display extension can
  add a head badge, each with a clean fallback when disabled or malformed.
- No plugin call runs on the UI thread; a runaway provider is trapped and the
  window stays responsive.

---

## 12. Open questions

1. **WASM engine.** The recommendation is a pure-Rust interpreter such as
   `wasmi`: no JIT, fuel metering, and no C runtime. Is interpreter speed
   enough for parsing large messages, or should the host parse more and the
   module parse less? WASM stays in-process; a separate plugin process is out
   of scope.
2. **Host network surface.** How much of `http_request` and the `net_*`
   stream API to expose, and how the endpoint allowlist is derived for a
   custom IMAP host. Too narrow and OAuth/STARTTLS cannot work; too wide and
   the sandbox only protects memory.
3. **Blob transport.** Inline base64 is simple; blob handles are efficient.
   Freeze the exact blob calls before the first Gmail/Graph attachment work,
   or attachments will grow two implementations.
4. **Local and native labels with the same name.** Keep them distinct records
   (this draft) or merge them visually? Distinct is correct for routing, but
   the UI must explain why one is local.
5. **Remote-first everywhere.** Is there a real offline mode? If yes, the
   remote-first rule needs a pending-operation journal, which this draft
   deliberately does not have. Recommendation: no offline queue in v1.
6. **Graph identity.** Does every supported Outlook.com/Microsoft 365 tenant
   honour immutable ids? If not, the fallback identity needs a migration rule
   before the first delta.
7. **OAuth browser flow.** Loopback redirect versus device code. Device code
   is simpler on Linux desktops; loopback is faster. Support both, pick device
   code as the default when no browser integration is available.
8. **Reference modules as `.wasm`.** Packaging must install them. Is a missing
   module a hard startup error for the affected account, or a warning? This
   draft says warning plus unavailable account; confirm.
9. **Extension budget.** 50 ms is a guess. Measure the built-in organizer and
   display on the reference machine before freezing it.
10. **Shared mailboxes and multiple identities.** Graph shared mailboxes and
    Gmail delegated accounts need a first-class identity model; not in v1.
11. **Plugin signing.** A checksum in a manifest is not a signature. If modules
    are distributed, signing and a key policy need their own design.

---

## Appendix A — a provider request and response

`message.set_labels` on Gmail:

```json
{
  "abi": 1,
  "method": "provider.invoke",
  "request_id": "01JAB...",
  "operation": "message.set_labels",
  "account": {
    "id": "work",
    "provider": "gmail",
    "config": { "host": "imap.gmail.com", "port": 993, "tls": "implicit" }
  },
  "credentials": {
    "kind": "oauth2",
    "access_token": "<redacted>",
    "expires_at": "2026-10-27T12:00:00Z"
  },
  "target": {
    "messages": [
      { "global": "1830000000000000001", "mailbox": "INBOX", "local": "4412" }
    ]
  },
  "params": {
    "add": ["Receipts"],
    "remove": ["\\Inbox"]
  },
  "mode": "native",
  "deadline_ms": 30000
}
```

```json
{
  "ok": true,
  "effects": [
    {
      "type": "labels.set",
      "identities": [
        { "global": "1830000000000000001", "mailbox": "INBOX", "local": "4412" }
      ],
      "labels": ["Receipts"]
    }
  ],
  "warnings": []
}
```

A failure keeps the local label unchanged:

```json
{
  "ok": false,
  "error": {
    "code": "rate_limited",
    "message": "Gmail asked us to slow down",
    "retryable": true,
    "retry_after_ms": 5000,
    "details": {}
  }
}
```

## Appendix B — an extension manifest

```json
{
  "schema": 1,
  "id": "org.example.sender-groups",
  "name": "Sender groups",
  "version": "0.1.0",
  "abi": 1,
  "module": "sender-groups.wasm",
  "default_enabled": false,
  "permissions": [],
  "providers": [],
  "extensions": [
    { "id": "sender-groups", "hooks": ["organizer.v1"] }
  ]
}
```

## Appendix C — glossary

| Term | Meaning |
| --- | --- |
| Host | Kimo, the process that owns the UI, store and scheduling. |
| Provider | A mailbox or submission implementation behind the ABI. |
| Extension | A declarative GUI contribution behind the same ABI. |
| Plugin | The Rust `.wasm` module that contains providers and/or extensions. |
| Native | Implemented by the provider against its server or API; not a native-code plugin. |
| Local | Implemented by the host in SQLite. |
| Effect | A validated local mutation returned by a provider. |
| Capability | A named promise an operation can be dispatched to. |
| Profile | A required capability set (`kimo-imap/1`, `kimo-graph/1`, …). |
| Generation | A mailbox identity epoch (IMAP UIDVALIDITY, delta reset). |
