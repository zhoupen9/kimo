Google Gmail
---

# protocol
Gmail extends the traditional IMAP protocol (which was built around a rigid, hierarchical folder structure) by introducing a proprietary extension capability known as **`X-GM-EXT-1`**.

Because Gmail's underlying architecture is built around a single flat datastore where messages have **labels** rather than living in physical folders, Google provides specialized attributes to bridge this gap.

The primary features Gmail provides to specialize IMAP include:

### 1. `X-GM-LABELS` (Label Management)

* **What it does:** Standard IMAP only understands folders (where a message can typically only reside in one folder at a time). Gmail uses `X-GM-LABELS` to expose Gmail labels over IMAP.
* **Capabilities:** Clients can fetch, add, or remove multiple labels on a message via standard commands (like `FETCH` or `STORE` with `+X-GM-LABELS`) without duplicating the underlying email data on the server.

### 2. `X-GM-THRID` (Thread IDs)

* **What it does:** Traditional IMAP treats every email as an independent entity; clients have to manually compute conversation threads by parsing headers (`In-Reply-To` and `References`).
* **Capabilities:** Gmail assigns a persistent, server-computed 64-bit integer **Thread ID** to every conversation. Clients can query `X-GM-THRID` to instantly group related messages together exactly the way they appear in the Gmail web interface.

### 3. `X-GM-MSGID` (Unique Message IDs)

* **What it does:** In standard IMAP, a message’s UID can sometimes change or be relative to a specific folder context.
* **Capabilities:** Gmail provides a globally unique, persistent 64-bit **Message ID** (`X-GM-MSGID`) for every email. No matter how many labels/virtual folders an email appears in, this ID remains constant, allowing clients to easily de-duplicate messages.

### 4. `X-GM-RAW` (Native Gmail Search Query Integration)

* **What it does:** Standard IMAP search capabilities are basic and limited to standard headers (`FROM`, `SUBJECT`, `SINCE`, etc.).
* **Capabilities:** Gmail extends the IMAP `SEARCH` command with `X-GM-RAW`, allowing clients to pass full, powerful Gmail search operators directly to Google’s search engine backend (e.g., searching for `X-GM-RAW "has:attachment larger:10M"` or `is:unread`).

### 5. `LIST`, the `[Gmail]` container, and labels as mailboxes

Gmail's IMAP `LIST` has one quirk that is not part of `X-GM-EXT-1`: the
system mailboxes are nested under a namespace container named `[Gmail]`,
returned as `\Noselect`.

```
* LIST (\Noselect \HasChildren) "/" "[Gmail]"
* LIST (\HasNoChildren \All) "/" "[Gmail]/All Mail"
* LIST (\HasNoChildren \Drafts) "/" "[Gmail]/Drafts"
* LIST (\HasNoChildren \Sent) "/" "[Gmail]/Sent Mail"
* LIST (\HasNoChildren \Junk) "/" "[Gmail]/Spam"
* LIST (\HasNoChildren \Flagged) "/" "[Gmail]/Starred"
* LIST (\HasNoChildren \Trash) "/" "[Gmail]/Trash"
* LIST (\HasNoChildren \Important) "/" "[Gmail]/Important"
* LIST (\HasNoChildren) "/" "Receipts"
```

`[Gmail]` is not a mailbox: `SELECT "[Gmail]"` is refused, so it must not be
offered as a folder. Its children are real mailboxes (`[Gmail]/All Mail`,
`[Gmail]/Drafts`, `[Gmail]/Sent Mail`, `[Gmail]/Spam`, `[Gmail]/Starred`,
`[Gmail]/Trash`, `[Gmail]/Important`), and every other name in the answer is
a user **label**, exposed by the same mechanism. That is why Kimo treats them
as *synthetic* mail folders: selecting one is selecting the label's view, not
a directory in Gmail's flat store, and one message carries several of them
through `X-GM-LABELS` without a copy per label.

The plugin drops the container (and any other `\Noselect` namespace) from
its `mailbox.list` answer and keeps the wire names of everything else.
Stripping the `[Gmail]/` prefix, the label glyph and the INBOX → system
folders → labels order are display rules on the host's side
(`docs/ui-shell.md`).
