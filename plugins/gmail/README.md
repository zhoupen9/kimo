# Gmail — the first real provider module

`kimo-gmail/1`: IMAP and SMTP behind XOAUTH2, with every `X-GM-EXT-1`
extension Kimo uses.

- **`X-GM-LABELS`** — fetched beside flags, written with
  `UID STORE ±X-GM-LABELS`: system labels keep their `\Trash`-shaped atom
  form, user labels are quoted strings. Native labels are the provider
  capability the host mirrors.
- **`X-GM-THRID` / `X-GM-MSGID`** — Gmail's own thread and message ids,
  returned with every message for native threading and a global identity.
- **`X-GM-RAW`** — `UID SEARCH X-GM-RAW "…"` for Gmail's own search language
  (`query.raw`, dialect `gmail`); CR/LF and oversized expressions are
  refused before the wire.
- **`LIST` and `[Gmail]`** — Gmail nests its system mailboxes under a
  `\Noselect` `[Gmail]` container. `mailbox.list` drops the container (and
  any other `\Noselect` namespace) because `SELECT` refuses them; the
  system folders keep their wire names (`[Gmail]/All Mail`, …) and the
  account's labels are the other names. The host's folder rows strip the
  prefix for display and draw labels with the label glyph.
- **XOAUTH2** — IMAP `AUTHENTICATE XOAUTH2` (including the base64 JSON
  error continuation Gmail answers a bad token with) and SMTP
  `AUTH XOAUTH2` for submission.
- **Degrade** — a server without `X-GM-EXT-1` is served as plain
  `kimo-imap/1`: no labels, no native threads, no Gmail search, delete via
  flags. `provider.health` says which one the account really has.

## OAuth2, and why the browser belongs to the host

The module cannot open a socket, a window or a process. The flow is split
the way the spec splits it:

1. the host binds a loopback socket and hands its port to `auth.begin`;
2. the module builds the authorization URL (PKCE S256, offline access,
   `https://mail.google.com/`) and returns it as an `open_url` challenge;
3. the host opens the user's browser on it — the XDG portal's `OpenURI`,
   `xdg-open` as the fallback — and waits for the redirect;
4. the code and state go back to `auth.continue`, which exchanges the code
   at `https://oauth2.googleapis.com/token` through its `net.http` import;
5. `auth.refresh` trades a refresh token for a fresh access token.

The state is consumed with the code, win or lose, so a finished sign-in
can never be replayed from a stale tab.

## Client id

Google issues OAuth client ids to the deployment, not to the software.
The module ships with none: `auth.begin` refuses to invent one and asks
for `client_id` in its params (the account form's config field in the
descriptor), which a deployment fills with a Desktop-type client from its
own Google Cloud project.

A **Desktop app** client also has a *client secret*, and Google's token
endpoint refuses the exchange without it (`client_secret is missing`) even
though the installed-app documentation calls the parameter optional. The
secret is public by design for an installed app — it ships inside every copy
of the binary — but it must be sent, so the account form has a second field
and the module passes whatever it is given as `client_secret` to both
`auth.continue` and `auth.refresh`. Entra's public clients have no secret,
which is why the field is optional rather than required.

App passwords are the separate `auth.password` path (IMAP `LOGIN`); they
never reach submission.

## Network

The module speaks IMAP on `imap.gmail.com:993` and submission on
`smtp.gmail.com:465`, over TLS, and nothing else — there is no HTTPS mailbox
API in it the way Graph is for Outlook. A network that filters those ports
(some do, while leaving 443 wide open) cannot sync Gmail with any client: the
connection is made and then dropped without a byte, and the module reports it
as `network: imap.gmail.com:993 closed before the greeting … the network may
block IMAP`. OAuth is unaffected — it is HTTPS — which is why a sign-in can
succeed on a network where the mailbox cannot.

## Building

```sh
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/kimo_plugin_gmail.wasm gmail.wasm
```

Install into a plugin directory (or point `KIMO_PLUGIN_PATH` here):

```sh
mkdir -p ~/.local/share/kimo/plugins/gmail
cp plugin.json ~/.local/share/kimo/plugins/gmail/
cp gmail.wasm  ~/.local/share/kimo/plugins/gmail/
```

The manifest grants `net.http`, `net.tls` and `secrets`, with a network
allowlist of exactly the four endpoints the provider needs:
`accounts.google.com` and `oauth2.googleapis.com` over HTTPS,
`imap.gmail.com:993` and `smtp.gmail.com:465` over TLS. The host enforces
the list on every call.

The module's own tests (`cargo test`, no target flag) run the parsing and
formatting half on the host: the PKCE vector from RFC 7636, the XOAUTH2
string, FETCH lines with literals and labels, quoting, dot-stuffing, and
the envelope of every operation. The socket paths are exercised by Kimo's
own tests, which load `gmail.wasm` through the real ABI.
