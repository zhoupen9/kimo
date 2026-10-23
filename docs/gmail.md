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
