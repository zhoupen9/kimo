# Outlook / Microsoft 365 — the Graph provider module

`kimo-graph/1`: Microsoft Graph over the host's HTTPS import. No IMAP, no
SMTP, no sockets — `net.http` and `secrets` are the whole manifest, and the
network allowlist is exactly two hosts: `login.microsoftonline.com` and
`graph.microsoft.com`.

- **Authentication** is OAuth2 against Entra ID: authorization code + PKCE
  (a public client, no secret), scopes `offline_access Mail.ReadWrite
  Mail.Send`, the browser leg run by the host (`src/plugin/auth.rs`). There
  is no password path — Microsoft switched basic auth off.
- **Reading** is POP3-shaped on purpose: the host hands the module the
  message ids its database already holds (`sync.stored_uids`), the module
  lists the newest page (`$top`, `receivedDateTime desc`), filters what is
  new, and fetches each one's raw source with `GET …/$value` — the same
  bytes an IMAP `BODY[]` would have returned, so the host's parser is
  unchanged. `Load older mail` pages down with `$skip`, and the folder mark
  counts pages, because Graph pages rather than numbers its mail.
- **Folders** are listed two levels deep (`mailFolders` + one
  `childFolders` walk, `@odata.nextLink` followed). The host's folder rows
  are keyed by *display* names; Graph's stable handle is the folder id, so
  every operation resolves name→id through a listing, first match wins.
  Two folders with one display name is the module's honest limitation until
  the store grows a folder-id column.
- **Flags** are `PATCH /me/messages/{id}` — `isRead`, and
  `flag.flagStatus`. **Delete** is `DELETE`, which is Graph's own soft
  delete into Deleted Items. **Search** is `$search` (KQL, dialect
  `graph-kql`); CR/LF and oversized expressions are refused before the wire.
- **Sending** is `POST /me/sendMail` with `saveToSentItems: true`. Graph
  takes a message object, not MIME, so the host's rendered RFC 5322 bytes
  are parsed back (mail-parser) into subject, body and `fileAttachment`s.
  Because Graph files the Sent copy itself, the module declares
  `send.save_copy` — and the host then knows not to append a second one.
- Every request sends `Prefer: IdType="ImmutableId"`, so a message keeps
  its id when it moves. A 429 carries Graph's own `Retry-After` back as
  `retry_after_ms`.

What is deliberately *not* declared: `message.append` (Graph files sent
mail itself), `message.set_labels` (categories are their own mapping, not
yet spoken), `thread.native` (`conversationId` is not returned with a page
yet). A capability is a promise.

## Client id

Entra issues client ids to the deployment: register an application with a
"Mobile and desktop applications" redirect URI of `http://localhost`, and
put its id in the account form's client-id field. The module ships with
none and `auth.begin` refuses to invent one. `common` serves both personal
Outlook.com accounts and Entra tenants; a deployment that wants a single
tenant replaces `common` in `oauth.rs` with its tenant id.

## Building

```sh
cargo build --release --target wasm32-unknown-unknown
cp ../target/wasm32-unknown-unknown/release/kimo_plugin_outlook.wasm outlook.wasm
```

Install into a plugin directory (or point `KIMO_PLUGIN_PATH` here):

```sh
mkdir -p ~/.local/share/kimo/plugins/outlook
cp plugin.json ~/.local/share/kimo/plugins/outlook/
cp outlook.wasm ~/.local/share/kimo/plugins/outlook/
```

The module's own tests (`cargo test`, no target flag) run the parsing and
formatting half on the host: the PKCE vector from RFC 7636, the Entra
authorize URL, folder and message page parsers, the sendMail message built
from a rendered MIME (attachment included), and every envelope shape. The
HTTPS paths are exercised by Kimo's own tests, which load `outlook.wasm`
through the real ABI.
