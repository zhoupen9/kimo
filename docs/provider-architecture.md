# Providers and extensions — the abstraction Kimo runs on

Read when designing a provider plugin, an extension hook, or a change to how the
UI talks to mail. The contract itself is [mail-spec.md](mail-spec.md); this page
is the mental model behind it.

Kimo is split into four concerns:

- **UI** — draws the window and turns clicks into actions.
- **Host** — owns scheduling, storage, credentials, permissions, network policy
  and validation.
- **Providers** — WASM modules that implement mail operations against a server
  or API.
- **Extensions** — WASM modules that only describe how mail should be grouped
  or displayed.

The abstraction is that Kimo does not know whether an account is IMAP, POP3,
Gmail or Graph. It knows **operations**, **capabilities** and **effects**.

## 1. The big picture

```text
┌──────────────────────────────────────────────────────────────┐
│                           Kimo UI                            │
│  mailbox list · message list · reading pane · compose card  │
└──────────────────────────────┬───────────────────────────────┘
                               │ user action
                               │ "label this", "delete that",
                               │ "show this mail", "send this"
                               ▼
┌──────────────────────────────────────────────────────────────┐
│                         Kimo host                            │
│                                                              │
│  action resolution · capability checks · scheduling          │
│  credentials · endpoint policy · limits · validation         │
│  SQLite store · sync jobs · events back to the UI            │
└───────────────┬───────────────────────────────┬──────────────┘
                │                               │
                │ provider operation            │ extension hook
                ▼                               ▼
┌──────────────────────────────┐   ┌───────────────────────────┐
│      Provider plugin         │   │     Extension plugin      │
│      (.wasm)                 │   │     (.wasm)               │
│                              │   │                           │
│  IMAP / POP3 / SMTP          │   │  organizer.v1             │
│  Gmail X-GM                  │   │  display.v1               │
│  Microsoft Graph             │   │                           │
└──────────────┬───────────────┘   └──────────────┬────────────┘
               │                                  │
               │ host imports                     │ declarative plan
               │ HTTP / TLS / blobs               │ no egui, no DB
               ▼                                  ▼
┌──────────────────────────────┐   ┌───────────────────────────┐
│   Host network backend       │   │  Validated view plan      │
│   DNS · TCP · TLS · HTTP     │   │  groups · badges · rows   │
│   allowlists · timeouts      │   │  head sections · content  │
└──────────────┬───────────────┘   └──────────────┬────────────┘
               │                                  │
               ▼                                  ▼
        mail server / Graph                  Kimo UI
```

The host is the only component that sees all three sides: the UI, the local
database and the plugins.

## 2. One action, end to end

For example, the user applies a “Receipts” label to a message.

```text
User clicks "Receipts"
        │
        ▼
Kimo UI builds an operation:
  "message.set_labels"
  target: message m1
  params: add "Receipts"
        │
        ▼
Kimo host resolves the operation
  ├─ Is this a local label?       ──► update SQLite, emit event
  ├─ Does the provider have
  │  message.set_labels?          ──► call provider plugin
  └─ No provider capability?      ──► explicit "unsupported" or safe fallback
        │
        ▼
Provider .wasm
  ├─ Gmail:  UID STORE +X-GM-LABELS
  ├─ Graph:  PATCH /me/messages/{id}
  └─ IMAP:   no native label, local-only fallback
        │
        ▼
Provider returns effects:
  "labels.set" for message m1
        │
        ▼
Host validates effects
  ├─ account owns message?
  ├─ labels valid?
  └─ provider declared this capability?
        │
        ▼
Host commits one SQLite transaction
        │
        ▼
Host emits MessageChanged
        │
        ▼
UI redraws the row
```

The provider does **not** write the database. It says what should change; the
host decides whether the change is valid and applies it.

## 3. Local, protocol and vendor operations

The same abstract operation can resolve in three ways:

```text
                    message.set_labels
                            │
          ┌─────────────────┼─────────────────┐
          ▼                 ▼                 ▼
       Local            Protocol           Vendor
   SQLite labels      IMAP flags       Gmail X-GM-LABELS
   no network         UID STORE        Graph categories
```

- **Local** — the semantics are Kimo’s own, such as local labels on plain IMAP.
- **Protocol** — a standard command, such as IMAP `\Seen` or POP3 `DELE`.
- **Vendor** — a provider-specific operation, such as Gmail labels or Graph
  categories.

A single operation may combine them. A delete can be “native remote delete”
followed by “local row removal.” A send can be “SMTP submit” followed by “IMAP
APPEND to Sent.”

## 4. Providers are capability subsets

Kimo defines profiles, and a provider declares which ones it supports.

```text
kimo-core/1
  mailbox.list · mailbox.sync · message.list · message.fetch
        │
        ├── kimo-imap/1
        │     flags · delete · append
        │     optional: move · search · threads
        │
        ├── kimo-pop3/1
        │     one mailbox · delete
        │     local flags · local labels · local thread
        │
        ├── kimo-gmail/1
        │     IMAP + X-GM-LABELS · X-GM-THRID · X-GM-RAW
        │
        └── kimo-graph/1
              Graph folders · messages · categories
              conversations · delta · send

kimo-smtp/1
  send.submit only
```

A POP3 plugin is still a complete Kimo provider. The UI simply hides or disables
operations the provider does not declare.

## 5. The WASM boundary is the contract

The host and plugin do not share Rust types or native function pointers.

```text
┌──────────────────────────────┐
│ Provider / extension .wasm   │
│                              │
│ exports:                     │
│   kimo_abi_version()         │
│   kimo_alloc(len)            │
│   kimo_free(ptr, len)        │
│   kimo_invoke(ptr, len)      │
│   memory                     │
└──────────────┬───────────────┘
               │ UTF-8 JSON in linear memory
               ▼
┌──────────────────────────────┐
│ Kimo host                    │
│                              │
│ imports:                     │
│   log · now · cancelled      │
│   random                     │
│   http_request               │
│   net_open / net_starttls    │
│   net_read / net_write       │
│   blob_read / blob_write     │
└──────────────────────────────┘
```

The module never gets a database handle, a socket, a file descriptor, an egui
context or an environment variable. It gets only the imports the manifest
allows.

## 6. Extensions only change presentation

Extensions are deliberately weaker than providers.

```text
┌──────────────────────────────────────────────────────────┐
│                    Message list                          │
│                                                          │
│  host default: thread by References / In-Reply-To        │
│        │                                                 │
│        ├── organizer.v1 plan                             │
│        │     "group by sender"                           │
│        │     "group by label"                            │
│        │     "annotate VIP senders"                      │
│        ▼                                                 │
│  validated groups + badges                               │
└──────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────┐
│                    Reading pane                          │
│                                                          │
│  host default: From / To / Date / Subject / body         │
│        │                                                 │
│        ├── display.v1 plan                               │
│        │     extra head sections                         │
│        │     badges and actions                          │
│        │     text / HTML / parts content                 │
│        ▼                                                 │
│  host draws the plan using its own theme and widgets     │
└──────────────────────────────────────────────────────────┘
```

An organizer gets message summaries only. A display extension gets the parsed
mail. Neither can draw, mutate the store, send mail or open a socket. An
extension returns a plan; the host validates it and draws it.

## 7. The runtime layers

```text
                    ┌────────────────────┐
                    │     Kimo UI        │
                    └─────────┬──────────┘
                              │ actions / plans
                    ┌─────────▼──────────┐
                    │     Host core      │
                    │  operations/events │
                    │  store/policy      │
                    └───────┬───────┬────┘
                            │       │
              ┌─────────────▼─┐   ┌─▼──────────────┐
              │ WASM engine    │   │ SQLite         │
              │ wasmi          │   │ mail + labels  │
              └───────┬────────┘   └────────────────┘
                      │
          ┌───────────┴────────────┐
          ▼                        ▼
   Provider module          Extension module
   (mail operations)        (view plans)
          │
          ▼
   Host network backend
   HTTP / TLS / STARTTLS
          │
          ▼
   IMAP / POP3 / SMTP / Graph
```

The engine is pure Rust (`wasmi`), the network backend is host-owned, and the
database remains the only C dependency.

## 8. The mental model

A useful shorthand:

```text
Kimo = host + operation vocabulary + local store + UI

Provider = "what mail operations mean on this server"
Extension = "how this mail should be grouped or shown"

Host owns:        truth, transactions, policy, credentials, network, scheduling
Provider owns:    remote semantics and wire/API translation
Extension owns:   presentation plans, never state
WASM boundary:    data + capabilities, never pointers into the host
```

That is the abstraction: Kimo runs **operations over providers** and
**presentation plans over extensions**, with the host validating and mediating
every crossing.

## 9. Current implementation status

The host/ABI side lives in `src/plugin/`; the full contract and its current
limitations are in [mail-spec.md](mail-spec.md). The network backend is real
(`StdNetwork` in `src/plugin/net.rs`) and is handed to an instance at
instantiation, so startup discovery still probes with the deny-everything
backend while a plugin-backed account gets sockets; the OAuth browser flow is
`src/plugin/auth.rs` (loopback redirect, the desktop's OpenURI portal with an
`xdg-open` fallback). [`plugins/gmail`](../plugins/gmail/) is the first real
provider: XOAUTH2 for IMAP and SMTP, all four `X-GM-EXT-1` extensions, Gmail
submission, and the authorization-code flow above. The existing account path
still opens IMAP/POP3 through [providers.md](providers.md) — a plugin-backed
account is not wired yet. The abstraction above is the target the host is
built for, not a description of every live path today.
