# Architecture — threads, jobs and events

Read when touching `src/sync.rs`, the `Job`/`Event` enums, or anything that moves data between the UI thread and a worker.

## Data flow

```
UI (app.rs ui)
   │  spawn(Job)                                   UI thread — no sockets, no DB writes
   ▼
worker thread (sync.rs)
   │  ImapClient / Pop3Client / SmtpClient         blocking I/O is fine here
   │  db::save_messages / db::set_seen             the only place the DB is written
   ▼
Event (mpsc) ──► drain_events() at frame start ──► merge into AccountUiState
```

A worker also calls `wake::now()` after sending its event. While the window is
hidden there is no frame to start: egui can consider an immediate repaint
already served, and a Wayland window hidden to the tray has been dropped
altogether. `src/wake.rs` is the event loop's own channel, and a hidden
`KimoApp::pump_hidden` drains the channel's events and keeps the auto-sync
clock turning. Tray Show and Quit use the same wake.

## Sync is incremental

- Syncing is incremental, and the state that makes it so lives in the database:
  a folder carries its marks (`folders.uid_next` and `folders.first_num`, read
  through `db::FolderSync`). IMAP resumes at `uid_next` — `UID FETCH <next>:*`,
  the `by_uid` half of `imap::fetch_bodies`; a mark from another UIDVALIDITY
  generation is dropped and the folder fetched afresh — while POP3 asks for the
  UIDLs it does not already store (`db::stored_uids`). `db::save_messages` moves
  an IMAP folder's mark to one past the highest UID it holds, in the same
  transaction as the batch, so a crash can never advance past mail that did not
  reach the disk. `BACKFILL_LIMIT` (in `sync.rs`) is the size of a folder's
  *first* fetch: the newest messages only, and the rest waits for the button in
  the `Load older mail` button below. `Job::Sync` (startup
  and the title-bar button) fetches the folder list, INBOX and every folder that
  has been fetched before, and a folder that fails does not cost the others
  theirs (the first error is reported at the end). A folder the user never
  opened has no mark and waits for its first click, which is where its backfill
  happens; a warm folder with nothing new neither writes nor emits. Each
  batch it emits carries `new_mail` — true only for mail the fetch found
  beyond the folder's mark, so a backfill, a `Load older mail` and the copy
  of a mail just sent are all false — which is what a desktop notification is
  raised from (see the notifications section in [platform.md](platform.md)). **Syncing is
  the app's own
  business, and there is no mode that switches it off**: the startup sweep and
  the fetch-on-first-open always happen, and the title bar's Sync cell — always
  there, beside the cog — asks for the same sweep at any time. It sweeps every
  account and, for the one whose folder is open, fetches that folder even when
  it has no mark: the column says "Not fetched yet — click Sync in the title
  bar" for exactly that folder, and a folder with no mark is not part of a
  sweep — `sync_queue` is what puts it first, so the message is not a lie.
  The only sync nobody presses for is the clock in [preferences.md](preferences.md),
  and that is a preference which is off unless it is asked for.

- Older mail is fetched on demand: the message list floats a `Load older mail`
  button over its foot when the list is at its end and the folder's marks say
  the server has mail below everything here, and one click is a `Job::LoadOlder`
  of `OLDER_BATCH` (50) messages. Two marks answer that without asking the
  server on every scroll: `folders.first_num`, the floor a folder has been
  fetched *down to* (IMAP: the lowest UID a fetch was sent for; POP3: the lowest
  message number), and `folders.oldest_uid`, how far down the server goes — what
  `FETCH 1 (UID)` answered, which every IMAP sync records, and 1 for POP3, whose
  numbering starts there. `db::older_available` is the whole rule,
  `db::load_mailboxes` derives it at startup and `Event::OlderMail` carries a
  fresher answer. A click re-asks before fetching, so an emptied mailbox clears
  the button rather than failing on it; the floor moves even when the batch came
  back empty (the mailbox has gaps), which is why it is a mark about what was
  *asked for* — deletes leave it alone, and a UIDVALIDITY rotation clears it.

- The folder list is cached like the mail is, and the `LIST` is only what
  updates it. `folders.listed` records which mailboxes the last answer named;
  `db::save_folder_list` writes it in one transaction with the folders
  themselves — the ones the answer left out are marked unlisted, never deleted,
  because the mail cached for them is still the only copy of it here — and
  `db::load_mailboxes` carries the flag into `AccountUiState::folders`, which is
  what the navigation column opens on. So a start shows the folders the last
  `LIST` named instead of standing on an offered INBOX until a sync runs, and a
  sync that finds the same mailboxes changes nothing on screen. Two things are
  deliberately *not* a mailbox list: an answer that names no mailbox at all
  (`imap::list_folders` reports it as the protocol error it is, rather than
  saying the account has no folders), and one whose `LIST` **failed** — that
  leaves the stored list and its flags alone and reports the error on the
  account, while the sweep falls back to `db::listed_folders` — the mailboxes
  the last answer named — so a server that will not list its mailboxes can still
  have their mail fetched. That list is a fallback, not a display source: the
  column draws what `LIST` last said. Mail fetched for a folder proves the
  server has it, so `save_messages` lists it again — the one way back.

## Merge identity

- Merge identity: IMAP messages are keyed by `(UIDVALIDITY, UID)` per folder
  (a UIDVALIDITY change clears the folder); POP3 messages are keyed by their
  UIDL string, and `\Seen` is local-only. The database does the same in SQL:
  `db::save_messages` upserts on `(account_id, folder, uid)`, deletes a
  folder's rows first when its UIDVALIDITY moved, and keeps `seen` monotonic
  (`max(messages.seen, excluded.seen)`).

## The UI thread and remote resources

- The UI thread touches the database exactly once, in `KimoApp::new`
  (`db::load_accounts` + `db::load_mailboxes`, replacing the JSON parse that
  used to happen there); every frame after that works from the in-memory
  `AccountUiState`, and every write is a worker's (see `src/db.rs`).

- Remote HTML-mail resources (images, stylesheets) load by default — an
  explicit decision by the project owner, accepting that this leaks the
  reader's IP and open timing to senders (tracking pixels). Fetches run as
  `Job::FetchImage`/`FetchStylesheet` on worker threads via ureq (pure
  Rust, rustls + ring), are capped in size, cached on disk at
  `~/.local/share/kimo/cache/resources/` (SHA-1 of the URL) and held in
  `KimoApp::images`/`stylesheets`. The renderer reports missing URLs via
  `html::HtmlContext`; never fetch on the UI thread.
