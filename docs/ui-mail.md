# UI — the message list, the reading pane and mail actions

Read when touching a list row, threading, select mode, search in the bar, the reading pane's card, attachments, labels, flags, filters or sort.

## The message list

- A message row (`message_card`; the folder list and the search results share
  it, only the third line differs — the preview, or *account · folder — match*)
  separates unread mail by *weight*, not by color: the sender
  and the subject always take the scheme's `text` (read mail is not dimmed),
  the preview always takes `text_weak`, and an unread row's sender and
  subject are struck twice half a point apart (`paint_row_text`, see
  `BOLD_STRIKE`). That strike is a deliberate stand-in for real bold: epaint
  has no synthetic bold (`FontTweak` only scales and shifts glyphs) and the
  picked face may ship no bold at all — Adwaita Sans, the desktop default
  here, has only Regular and Italic, which is why the fonts module's
  one-face-per-family model cannot supply one. The second strike is sub-pixel
  and does not change the layout the galley was measured for, so the ellipsis
  cut and the right-aligned timestamp stay exactly where they were. The row's
  font sizes are the `ROW_DATE_FONT_SIZE` / `ROW_TITLE_FONT_SIZE` /
  `ROW_PREVIEW_FONT_SIZE` constants next to `MESSAGE_ROW_HEIGHT`, and where
  they sit are `ROW_SENDER_Y` / `ROW_SUBJECT_Y` / `ROW_PREVIEW_Y` (a 68 pt
  row, its three lines placed as one block centred in it: the room above the
  sender's line is the room below the preview, which
  `the_rows_three_lines_are_centred_in_the_row` pins), so retuning the list is
  one edit instead of a hunt for literals. The three lines are elided by
  epaint itself (`truncate_galley`: one `LayoutJob` whose `max_rows` is 1 and
  whose overflow character is the ellipsis), never by a search that lays a
  galley out per probe to measure it: the list re-lays out every visible row on
  every frame the column's width moves — a splitter drag, the navigation
  column's slide — and one shaping pass per line per frame is the whole of what
  that costs (`bench_nav_slide` measures it). A search's probes are largely
  cache-warm from the width of the frame before, but the frames where they are
  not pay `log2(chars)` shapings apiece — its worst frames cost about three
  times what the elision's do. A mail with parts gives the end of its title line
  to a paperclip (`paint_clip_icon`, one stroked path in the title bar's own
  24×24 outline style, drawn in the row's `text_weak` however unread it is),
  and the title is elided `ROW_CLIP_WIDTH` + `ROW_CLIP_GAP` earlier for it;
  `ROW_CLIP_SIZE` is the box the glyph is drawn in, which is taller than it is
  wide because a clip is. Both lists answer the same question — a folder row
  from `MailMessage.attachments`, a search hit from the count the query
  returned — so a hit no longer spells the paperclip out in its third line: the
  glyph is the one way the list says it.
  **A row offers its own delete button under the pointer**: a `ROW_ACTION_SIZE`
  (24 pt) cell at the right end of its first line, where the timestamp sits —
  the timestamp, and a conversation's count before it, gives way to a muted
  trash glyph (`paint_delete_icon` at `ROW_ACTION_GLYPH`), which takes the
  scheme's `danger_hover` fill with the `on_danger` glyph once the pointer is on
  the button itself — the treatment the title bar's close button gets, for the
  same reason: the button is the confirmation, and what it does cannot be
  undone. What is *laid out* on that line does not change with the hover — the
  sender is elided against the timestamp's slot either way — so nothing on the
  row moves under the pointer; only what is painted does. The button is a
  target on every row, drawn or not, like the box: a target that only appears
  once the pointer is already on it is one a click can miss. **What it takes is
  the mail the row speaks for** — a mail row's own mail, a conversation's
  *newest* one, which is the mail the row opens in the pane and the one the
  title bar's Delete would
  take — and not the whole thread: that is the row's box (all of it at once) or
  the pane's mail through the title bar (one at a time). A conversation's hover
  text says the newest mail outright; the title bar's own wording is used for a
  mail. A search hit's button is keyed by the hit, so it deletes where the hit
  lives and never opens the folder instead. It is offered in **both modes** — a
  row is a row — so select mode's box checks and its button still takes that one
  mail, without leaving the mode or touching the selection.
  **Inside that cell sits the row's flag button** (`paint_flag_icon`, the same
  pennant the row draws at the end of a flagged mail's title line): one
  `ROW_ACTION_SIZE` cell one `ROW_ACTION_GAP` further from the row's edge, so
  delete stays outermost as it is in the title bar and on the mail's own head,
  and a target on every row the way the other two are. **It is the title bar's
  action and not a second one** — `KimoApp::toggle_flag_on`, the same
  `Job::SetFlag` that cell and the pane's head run — so what it marks is the mail
  the row speaks for, and what it says is the bar's own wording, turning over
  with the state ("Flag …" / "Unflag …") and, for a conversation, naming the
  newest mail it marks the way its delete button beside it does
  (`row_flag_tooltip`). The state it switches is on the mail already — the
  warning fill and the pennant on the title line — so the glyph is drawn in the
  scheme's `warn` while the mail is flagged and in `text_weak` while it is not,
  and turning the pointer on the cell previews the mark by filling it with the
  same `flag` tint a flagged row itself is filled with. A click on it is not a
  click on the row.

- The message list is a list of **conversations** by default: `crate::threads::group`
  turns a folder's mail into threads — a reply joins its parent through
  `References` read from the end (the last id this folder holds is the parent),
  or through `In-Reply-To` when the client wrote no chain, and a mail whose
  headers say nothing joins the thread of the subject it answers (a prefix like
  `Re:` or `答复:` stripped, either colon; two mails that merely share a subject
  stay apart) — and the list draws one row per conversation. A conversation's row
  is its own: the newest mail's sender, date and preview under the thread's
  subject, the count of what it holds beside the date, a paperclip when any of
  its mails has one, and a chevron at the left: a thin line drawing kept at a
  subdued strength of the row's own weak color (`THREAD_CHEVRON_ALPHA`), coming
  up to the full color under the pointer — a control at the edge of a row, not
  part of the mail. A conversation of one is not a conversation: it is a plain
  mail row, with nothing in the chevron's column, and the pane shows it the way
  it always has. **Every row is inset by that column**, conversation or not
  (`RowPlace::indent`): the avatars line up down the whole list, and a mail with
  nothing to open leaves the column as the placeholder it is rather than starting
  a step further left than its neighbours. Select mode puts a checkbox in each
  of those slots instead of the circle — see the select-mode bullet below. The
  one exception is a mail of an *open* conversation: it steps in `THREAD_STEP`
  further and draws a smaller avatar (`THREAD_AVATAR_DIAMETER`,
  `RowPlace::avatar`), which — with the chevron's column left empty above it — is
  what says at a glance that the mail belongs to the conversation over it. **There is no guide line**: no line is
  drawn down that column for a thread's mails, and the step and the smaller
  circle are the whole of the mark.
  **The row and the chevron do different things.** The *chevron* opens and closes
  the thread's mails under it in the list — and does nothing else, so the list's
  shape only ever changes when the user asks for it. The *row* opens the
  conversation in the reading pane: every mail it holds, oldest first, each on
  the pane's own card (`mail_card`) — the cards stacked with
  `CONVERSATION_CARD_GAP` of chrome between them and nothing drawn around
  them — so a thread reads as the mails it is made of rather than as one long
  document with three sets of headers in it. A mail on its own is that same
  card, once: same edges, same width, which is what the pane's card test pins
  (`a_conversation_draws_one_card_per_mail_on_the_pane`). That is
  `KimoApp::open_conversation`, which takes the thread the selection is in
  from the list's own model; with nothing threading, or a model that does not
  cover the folder, the pane shows the mail alone. The pane's scroll offset is
  keyed by the *conversation* (`Conversation::key`), so opening another mail of
  the thread
  already on screen does not jump back to its top.
  The chevron is a click target of its own (`ui.interact` over the row's, with
  `RowResponse` reporting the two), it is a line drawing rather than the filled
  triangle the navigation column's sections use, and it is the only thing that
  writes `open_threads`, which is in memory only and keyed by `Thread::key` (the
  thread's oldest uid, stable while the thread is). Because the rows a frame drew
  are the ones it had before the click, a toggle asks for a repaint itself: a
  pointer that stops moving asks for no frame of its own, and the list would sit
  there looking as if the click was lost.
  An open conversation's mails are rows of their own under it, oldest first,
  each a step in with a smaller avatar and nothing drawn down that left column —
  `RowPlace` is what a row draws there, and only a conversation's own row draws
  anything in it: its chevron. Every row is
  `MESSAGE_ROW_HEIGHT` tall whatever it is, which is what keeps `virtual_rows`
  honest: threading changes *which* rows there are, not how tall one is. The rows
  are derived rather than built in the drawing: `KimoApp::list_rows` (`ListRows`)
  holds the threads and the rows until the folder's mail
  (`AccountUiState::folders_rev`, which a mail marked read moves too), the
  account or folder, the threading switch, or the set of open threads
  (`open_rev`) changes — grouping a folder is a map, a union-find and a sort over
  all of it, and the list is drawn on every frame.
  **Search results are deliberately not threaded**: they are a flat list of hits,
  and where a hit sits among its own folder's conversations is not what was
  asked for. Threading is a preference (`threaded` in settings.json,
  Preferences → General) and per folder, like the rest of the list.

- The message list has **two modes**, and which one it is in is what a click on
  a row means. **View mode** is the list as it always was: the row opens the mail
  — or the conversation — in the reading pane, and the avatar stands at its left.
  **Select mode** is the list as a checklist: every row draws a checkbox in the
  avatar's slot instead, the box takes checks, and a click anywhere else on the
  row leaves the mode instead of opening a mail — the way out sits beside the way
  in, because the mode is a temporary errand rather than a place to be.
  The way *in* is the pointer: in view mode the row under it trades its avatar
  for an empty box — `CHECKBOX_SIZE` (18 pt) in the 32 pt slot the circle fills,
  since a box that size reads as a control while the circle's size would read as
  a button — and a click on that box is what turns the mode on and checks the
  mail the row stands for. The box is a click target on **every** row whether or
  not it is drawn (`ui.interact` over the avatar's slot, registered after the row
  so it is the topmost of the two), because egui hit-tests a pass against the
  widget rects of the previous one: a target that only appears once the pointer
  is already on it is a target a click can miss. Hovering the box is hovering the
  row — the row's fill follows `resp.hovered() || checkbox.hovered()` — so the
  box is a control inside the row and not a hole in it, and a click on it is
  never a click on the row: the row's own `clicked()` is false when the box was.
  The box is drawn by `paint_checkbox`: off is a `text_weak` outline that takes
  the accent under the pointer, on is an accent fill with the scheme's `on_accent`
  check on it, and a conversation with only part of its mail checked is the same
  fill with a dash rather than the check. A checked row takes the whole-row
  `selection` fill the mail open in the pane already has, so what is checked is
  visible past the 18 pt box; a partly checked conversation's own row does not,
  and says what it is with the dash alone.
  **Select mode also has a select-all row**, pinned under the title bar over the
  top of the list: the same box in the same column, a label that says what a
  click would do (`Select all` / `Clear selection`), and a hairline under it. It
  is a band of its own height (`SELECT_ALL_HEIGHT`, 32 pt), drawn *after* the
  rows the way the floating `Load older mail` button is (`select_all_band`, off
  the scroll area's own viewport): the list scrolls **under** it, so the one
  control that is about the whole list never scrolls out of reach. It is opaque
  — the list's own surface, filled edge to edge like a row — which is what the
  rows sliding under it need. The frame's top margin grows by its height while
  the mode is on, so at rest the first row
  starts below it and `virtual_rows` keeps its uniform-row arithmetic — the title
  bar's own padding stays, so mail still passes under the glass above the band.
  It is not the list header the column deliberately has none of — it exists only
  while the mode does. The whole band is one click target, unlike a mail row's
  box: there is nothing else a click on it could mean, so a mis-aimed click on
  the label must not be what leaves the mode. Its box says `CheckState` the way a
  conversation's does — off, the dash for part of the list, the check for all of
  it — and the label turns into the way out when the click would clear.
  **It is about the list and not about the rows**: the keys are every mail the
  folder holds — closed conversations included — or every search hit, built on
  the click and never per frame. The drawn state is one comparison, the
  selection's size against the list's, and that is exact only because the
  selection is always a subset of the list on screen: `cancel_select` on every
  list change as before, and two guards for the ways a checked key could leave
  the list without one — a fresh search answer drops the keys it no longer names
  (`Event::SearchResults`), and a UIDVALIDITY rotation drops its folder's
  (`Event::Messages`), since those UIDs are meaningless with the mail they
  named.
  What is checked is a `MailKey` — account, folder and uid — and not a uid, since
  a search lists hits from every folder and account at once. `select_mode` and
  `selected_mails` are in memory only, the way the open conversations are, and
  the column is a list *of* something: whatever moves it to another list — the
  navigation's folder click, the search popup applying a different query or
  Clearing it, an account added, edited onto another endpoint or removed — ends
  the mode and drops the checks (`cancel_select`), so nothing is ever checked
  that is not on screen. A conversation's own row stands for every mail it holds,
  so its box takes the whole thread in or out at once; checking a mail in the
  list *under* it is still one mail at a time, and the conversation's box then
  says `Partial` rather than lying about what is in.
  Select-all takes what the list shows rather than what the folder holds: a
  view filter that hides mail must not have it checked by a click on that row
  (`ListRows::shown_indices`; the label and search lists do the same over their own
  rows).
  In select mode the mail actions' Delete is the only live cell
  (`mail_action_enabled`): reply, reply all, forward, label, flag and export are
  all about the one mail the reading pane shows, which is not what the click was
  about, so they are drawn dimmed with a tooltip saying so. A checked selection
  is also the one thing that keeps the run on the bar with no mail open — the
  pane's row waits for a mail otherwise (`titlebar_anchors`). Delete takes the
  selection — `delete_batches` groups the checked mail by account and folder,
  since a selection made over a search's hits spans folders, and each group is
  one `Job::DeleteMessage` on one connection — the rows go as the events come
  back, and a mail the server refused keeps both its copies and its check.
  Nothing about the mode is persisted, and there is no shift-click range and no
  Delete-key shortcut: a selection is made one checkbox at a time, or all at once
  with the select-all row.

## Search in the title bar and its popup

- Search lives in the title bar and in the popup it opens, never in the
  column. The title-bar box shows the query with the magnifier drawn inside its
  left edge, and the whole box is one click target: a click anywhere in it —
  glyph or text — opens the popup, as does Ctrl+F. The field in it is
  deliberately *not* interactive (it displays the query; the popup is the
  editor): a caret there would fight the popup's own field for the keyboard and
  leave focus behind when the popup closes, and one string backs both, so
  whatever is typed in the popup is what the box shows. `search_modal`, an
  `egui::Modal`, holds the free text, the scope combo and the filter grid
  the column used to carry, with the live result count along its bottom. **The
  popup is that bar and its form and nothing else**: no heading over it and no
  close button beside it — Escape and a click outside close it — and no Search
  button anywhere, because the query runs as it is typed (`SEARCH_SETTLE`);
  Enter in any of its boxes only skips that wait and closes. The bar carries the
  title bar's own magnifier inside its left edge (`paint_search_icon`) and,
  while there is text to clear, a round cross inside its right
  (`paint_close_icon` over a `scheme.hover` circle): the cross empties the
  bar's text and nothing else — each filter has a box of its own to empty — and
  the list behind follows at once rather than on the settle clock. The result
  row is part of the popup only while there is a query; with none, the form ends
  under the filters. The filter boxes are `Field::quiet`, the way the compose
  card's address rows are, and the scope is a flat combo with the app's chevron
  where egui would put a triangle (`flatten_widgets`, `paint_combo_chevron`) —
  [`ui-style.md`](ui-style.md) both times. Nothing else about a
  search is drawn: the message list starts
  straight at the rows — the folder's mail or the hits — because a header over
  them was noise the project owner asked to drop, and the magnifier is the only
  sign left. (Select mode's select-all row is not that header: it is there only
  while the mode is.) It is grey while the box is empty —
  `visuals.weak_text_color()`, the color of the hint text it sits next to, so
  the box reads as an empty placeholder rather than a live control — and
  accent-colored once a query is what the column shows.
  Nothing about a query is persisted.

## The reading pane

- The built-in renderer keeps two things between frames in `KimoApp::body`
  (`BodyCache`), because a splitter drag moves the reading pane's width on every
  frame and the whole body is otherwise re-derived on each one: the *parsed
  body* (`html::Document`, since html5ever's parse is the larger half of what a
  frame costs and the tree does not change while the mail is open) and the
  *width it settled at*. There is one entry per mail on screen — a conversation
  shows several, and the pane asks for every one of them on every frame — up to
  `BODY_CACHE` of them, the mail off screen longest being the one a new body
  evicts. The body is laid out at that width while the pane is
  moving — in a scope of its own, because a ui never gets narrower than what it
  has already drawn (`set_max_width` widens the max back to the min rect) — so a
  drag clips the mail instead of re-shaping every run of text in it; once the
  geometry has held still for `GEOMETRY_SETTLE` it reflows once, for the width
  the pane came to rest at. A new mail, or a body re-fetched under the same uid
  (`body_fingerprint`), drops the document and is laid out at the pane's own
  width at once. Do not draw the body outside that scope, and do not "fix" the
  hold by narrowing the card's own ui.

- Attachments are cards under the body (`attachment_card`), one per part the
  mail named a file for, in the order the parts appear in it: the page glyph,
  the file name, and — under the name — the MIME type and the size, whichever
  of the two the mail was fetched with (`attachment_meta`; `human_size` counts
  in binary units, the way a file manager on this desktop does, and drops a
  `.0`). **The cards are one run, not one per line**: each as wide as the name
  and the type under it want — `ATTACHMENT_CARD_MIN_WIDTH` at the floor, the
  mail's own column at the ceiling — laid across that column and wrapped onto
  the next row where the next one does not fit (`horizontal_wrapped`,
  `ATTACHMENT_CARD_GAP` both ways), so a mail with ten parts is a block of cards
  rather than ten lines, and a pane too narrow for the run wraps instead of
  cutting it. **A name is drawn whole** — the card is measured from it
  (`layout_no_wrap`, `ATTACHMENT_NAME_FONT_SIZE`) rather than given a width to
  be cut down to, and only a name the column itself cannot hold falls back to
  `truncate_galley` with the tooltip carrying the whole of it. Both of a card's
  lines are smaller than the body text (11 pt and 9 pt): a card is a note about
  the mail, not part of it. The run is laid out in a scope of its own
  — the column the headers were drawn across, taken from the pane *before* the
  body is drawn — because a body wider than that column (a fixed-width HTML
  table) widens the ui as it lays itself out, and the cards belong to the mail,
  not to what the mail overflowed by. They are the
  last thing on the mail's card whichever renderer drew the body,
  because `mail_card` draws them after `mail_body` rather than inside it: a
  Blitz body is a raster a worker made, and the cards are not part of it.
  **The whole card is a link, and the name is drawn as one**: the scheme's link
  color, a line under it while the pointer is on the card, the card filled with
  the hover color and the pointer turned into a hand. A click is a download —
  `KimoApp::download_attachment` → `Job::DownloadAttachment` — and it is the
  round trip an export makes, one step further: what the database holds is the
  name, the type and the size (schema v6), never the bytes, so the worker fetches
  the mail's original source, takes the part out of
  it (`mail::attachment_bytes`) and writes the file into
  `storage::downloads_dir` — the directory an export with no dialog to ask
  writes into, the same one-line `Saved to <path>` notice and the same
  `create_new` numbering, through the writer they share (`write_file`; the
  download's own rules are `download_name`, which keeps the sender's file name,
  its last path component and a plausible extension, and `safe_stem` behind
  both).
  The click carries the mail, the part's *place* in the mail and the name the
  card showed; the worker re-parses the original and refuses unless both match
  — handing over the bytes of some other part is worse than refusing — and the
  refusal is the account's, prefixed "attachment:", the way "export:" and
  "delete:" are. Nothing is kept afterwards either: the bytes exist in the
  downloads directory and nowhere else, and mail the server no longer has cannot
  be downloaded at all, and says so rather than writing something close.
  The hover text shows the whole name where the column had to elide it, and the
  type and size under it. A part with no file name is not an attachment at all
  (`mail::collect_attachments`), and a mail cached before v6 is the name alone:
  no second line, rather than a separator with nothing after it.

## Mail actions

- Deleting mail (the title bar's Delete → `KimoApp::delete_selected_message` →
  `Job::DeleteMessage { uids }`) is **remote first, local after**, one mail at a
  time: the worker asks the server to drop each message and only then deletes
  that database row (`db::delete_message`), so mail that is still on the server
  never disappears from here and a deletion that fails — no connection, a
  read-only mailbox — leaves that mail's copies alone and reports why on the
  account (`Event::SyncFailed`, prefixed "delete:"). The list of uids is what the
  button was about: one mail from the reading pane, or every mail checked in
  select mode. A run is one connection, not one per mail — IMAP checks LIST and
  SELECTs once and then probes, flags and expunges each uid
  (`ImapClient::delete_messages`), POP3 is one session whose `QUIT` commits every
  `DELE` — and a uid the server refuses is that uid's alone: the rest of the run
  is still asked, each mail that went comes back as its own
  `Event::MessageDeleted`, and the first refusal is what the account carries.
  The reverse case is the other half of the rule: a mail
  the server no longer has is deleted **locally only**, without a command that
  could touch anything else. IMAP asks before it flags — the mailbox has to
  still be in LIST (a mailbox that *is* listed but refuses SELECT is an error,
  not a licence to drop the local copy) and then `UID FETCH <uid> (UID)` has to
  answer for the UID — and only then is it flagged `\Deleted` and expunged,
  with `UID EXPUNGE` naming the one message and the mailbox-wide `EXPUNGE`
  only as the fallback for a server without UIDPLUS (which answers BAD). POP3
  has no folders and no flags: the UIDL is the whole address of the message,
  `DELE <number>` goes out for it when the drop still answers for it, and the
  `QUIT` that `pop3_delete` always sends is what commits the deletion.
  The row leaves the list only when the job reports success
  (`Event::MessageDeleted`) — the job brackets its round trip with
  `SyncStarted`/`SyncDone`, so the account's spinner is what says the mail is
  on its way out — and that event is also where the reading pane is cleared if
  it was showing that mail, its cached Blitz pages go (`forget_blitz_message`)
  and an active search is asked again, since the hit list is a snapshot of the
  database. A half-finished delete is harmless: a mail whose remote copy went
  but whose local row survived is deleted again by the same button, which then
  finds nothing on the server and stops at the local half.
  **Deleting is permanent.** There is no Trash: IMAP expunges and POP3's `DELE`
  is the server's own hard delete, so the button does not ask first — the
  tooltip says what it does — and nothing in the app can undo it. Deleting to
  a Trash folder instead (IMAP `UID COPY` + expunge; POP3 cannot) is a decision
  for the project owner, not a fix to slip in. The folder's sync mark is
  deliberately not moved back: the mail is gone from the server too, and a mark
  that walked back to it would only make the next sync fetch it again.

- Exporting a mail (the title bar's Export → `KimoApp::export_selected_message`
  → `Job::ExportMessage`) is the one thing in the app that writes a mail out of
  it, and the one action that asks where. **The worker asks the desktop first**
  (`crate::portal::save_file`, the XDG portal's `SaveFile` — the same
  hand-written D-Bus conversation the attach card's chooser uses, on a worker
  because it does not answer until the user has), then fetches the message's
  original RFC 5322 source from the server — IMAP
  `UID FETCH <uid> (UID FLAGS BODY.PEEK[])` through `ImapClient::uid_body` (PEEK
  on purpose: fetching a mail to export it must not mark it read), POP3 `UIDL` +
  `RETR` through `Pop3Client::retr_by_uidl` — and writes it byte for byte where
  the dialog said. The order is the point three times over: a cancelled dialog
  costs no round trip and says nothing (closing a save is not a failure), and the
  account's spinner — which says the *server* is being asked — does not turn while
  the user is choosing a name. The job's `dir` is both the folder the dialog is
  suggested to open on and the destination when there is no dialog to ask
  (`storage::downloads_dir`, made on the first export): a session with no portal
  is not an export that cannot happen, and the fallback hides nothing, because the
  notice names the file it wrote.
  **The dialog's answer is the file's name**, extension and all: the dialog was
  drawn with the name in it, and appending a suffix to what the user left would be
  Kimo deciding something the dialog already asked. That file is written by
  `write_at` — through a hidden `.kimo<pid>_<n>.part` beside it and a rename
  into place, so that replacing a file (which the dialog has already asked
  about) leaves either the file that was there or the whole new one, and never
  half of either. The no-dialog writer is `write_file`, the download's too:
  `<stem>.eml`, numbered `<stem>-2.eml` and on when a name is taken, and
  *created* (`create_new`) rather than merely asked about, so two exports of the
  same mail cannot overwrite each other. The name is made from the subject, with
  what a file name cannot carry replaced and the whole capped at
  `EXPORT_STEM_BYTES`
  (`export_stem`; `slug` would ASCII-fy every non-Latin subject away) — offered to
  the dialog as well as used by the fallback.
  **The original is the point.** What is stored here is the parsed mail, with
  each attachment's name, type and size but never its bytes, so a `.eml`
  rebuilt from the database would not be the message the sender wrote, while
  this one is the message whole, attachments and all. The price is that mail the server no longer has cannot be exported at
  all, and says so rather than writing something close (`Event::SyncFailed`,
  prefixed "export:" — the local copy is not a substitute for the original).
  The job brackets its round trip with `SyncStarted`/`SyncDone` like a delete,
  and answers `Event::MessageExported { path }` — with the path, because the name
  may be the dialog's or the fallback's and only the worker knows which it got.
  The app raises a **desktop notification** for it (`exported_notification`,
  "Mail exported" / "Saved to <path>") rather than the timed line above the
  message list that a *downloaded attachment* and a finished send use
  (`list_banner`, the place the storage warning stands): the export's click was
  in the title bar, and the file it wrote is not in the list. Nothing is raised
  when the dialog was closed — nothing happened, so there is nothing to say.

## Labels, flags, filters and sort

- **A label is the user's own mark on a mail, and a filter of its own.** The
  two are one feature: what makes a label first-class is that the column can
  look at the mail that carries it, and what makes the column able to is that
  the mark belongs to the mail. A mail lives in exactly one folder — where the
  server put it — and carries any number of labels; the name is the whole of
  one, unique case-insensitively, so `Work` and `work` are the same label
  rather than two rows (the popup keeps the spelling it was made with).
  `MailMessage::labels` is the app's copy of them, filled by
  `db::load_mailboxes` from the join table and **never by a fetch**: nothing
  about a label is asked of or told to a server, which is why a re-synced mail
  keeps the marks it had here (the merge in `Event::Messages` takes the
  fetched mail's fields and the held labels).
  **A label is made by being used.** There is no "new label" form: the popup's
  box names one, and `db::set_labels` creates whatever row it needs as it puts
  it on the mail — one transaction, and the message's own rows replaced, so what
  a mail carries afterwards is the set that was asked for rather than something
  merged with what was there. The popup (`KimoApp::label_popup`) is the label
  cell's menu — `egui::Popup::menu` with `CloseOnClickOutside`, so ticking
  several labels is one visit — anchored to the cell that opened it, opened from
  the title bar's cell for the mail the reading pane shows and from the cell on
  a mail's own head for that mail (`mail_sheet`; the head's five cells are the
  bar's run with flag and export left out). Its checklist is every label, ticked
  for the ones the mail carries, and a tick or an untick writes the whole set
  through `Job::SetLabels` — the app's copy first, so the next frame draws it,
  and a worker to make it durable. Its box is emptied each time a popup opens:
  what was typed for one mail is not a draft for the next.
  **The mail says what it is labelled with**: a card draws its own labels as one
  run of chips under the head (`label_chips`), one per label and only while it
  has any, so an unlabelled mail's head is still the first thing on it.

- **A flag is the reader's own mark too, and the one mark the server keeps
  as well.** The two halves are what make it different from a label: it is one
  state per mail rather than a set (`MailMessage::flagged`, schema v9), and it is
  `\Flagged` on an IMAP server, so flagging a mail here flags it everywhere.
  The title bar's flag cell runs `KimoApp::toggle_flag`: the app's own copy of the
  mail is flipped first, so the warning fill appears under the pointer rather
  than a round trip later, and `Job::SetFlag` writes the database and pushes `UID
  STORE +FLAGS.SILENT (\Flagged)` — or `-FLAGS` to take it off — for an account
  that speaks IMAP. The local write happens first, like `mark_seen`'s: a server
  that refuses the `STORE` does not cost the reader the flag they just set, and
  the failure is reported on the sync cell like every other account failure. A
  POP3 account has nowhere to push it and keeps the flag in the database alone.
  **A fetch is where the flag can also arrive**: `MESSAGE_UPSERT`'s `INSERT` carries
  `flagged`, so mail the server reports as flagged is stored with the mark —
  and the column is in neither the `SET` nor the `WHERE`, so a mail the database
  already has keeps what the reader did with it. That is what makes a POP3
  re-fetch — which rewrites every message it holds, and has no flags to report —
  harmless.
  **A flagged mail is shown in the list, and only there.** Its row takes the
  scheme's `flag` tint and draws the flag glyph at the end of its title line
  (`message_card`, beside the paperclip for a mail that carries both), while the
  reading pane is untouched: the mail itself keeps its plain `content` card, so
  what a flag changes is where the mail is *listed*, not how the mail reads. The
  selection and the pointer are stronger than the mark and are painted over the
  row's fill — the fill is selected, else hovered, else flagged — which is what
  the glyph is for: it is the one thing on the row that says *flagged* whatever
  else the row is doing, and the bar's own cell says it in the same colour. A
  conversation's row stands for its mails and carries the mark if any of them
  has it (`Thread::flagged`), and a search hit carries it too
  (`SearchHit::flagged`, off the query rather than out of the search index), so no
  re-index is needed to see it.

- **The label filter is an AND, and it is the mailbox's rather than a
  folder's.** A click on a label row adds it to `KimoApp::label_filter`
  (`toggle_label`), a click on a selected one takes it out, and the message
  list becomes every mail that carries *all* of them — across folders and
  accounts, which is what "first-class filter" means here and why those rows are
  drawn like search hits (`labelled_row`: the third line is where the mail
  lives) and are flat rather than threaded. The list is derived rather than
  scanned per frame (`LabelView`, `refresh_label_view`, keyed by
  `labels_rev`, the accounts' `folders_rev` and the filter), the way the
  list's conversations are. Nothing about the filter is persisted: which labels
  you are looking at is a per-session thing, like select mode and the open
  conversations. A query and a label filter are two answers to what the middle
  column shows, so a label click ends an active search (`end_search`) — while
  a folder click, and opening a mail from the filtered list, deliberately leave
  the filter alone: the pane's folder is where the mail it shows is filed, not
  what the list is showing.

- **The list filter is a set of optional views, and they are an AND.** The title
  bar's filter cell opens a menu over whatever the column lists —
  `ListFilter::Unread`, `Flagged`, `ToMe`, `HasFiles`, each an independent row
  (ticking one never takes another off, and the menu stays open across ticks) —
  and a mail is listed only while it passes **every** ticked view
  (`ListFilters::matches`, an `all` over the set). A row is drawn by the app rather
  than by a stock checkbox (`filter_menu_row`): **the view's own mark at the left,
  its name, and the tick at the row's *end***, where a menu's check belongs — a
  checkbox puts its box in front of the name, which is where the mark goes. The
  marks are the list's own where the list has one (the flag, the paperclip),
  and an envelope, an envelope with the new-mail pip, and a person for the
  rest, all in the 24×24 outline style the whole icon set is drawn in. No tick at all is the menu's
  own **All** row: it is drawn ticked exactly while the set is empty and ticking
  it clears the rest, so "all mail" is the absence of views rather than one of
  them. The cell is accent-coloured while any view is ticked and its tooltip
  names them ("showing unread + flagged mail"). It applies
  to all three lists the column can show: a folder's mail (filtered before the
  conversations are built, so a conversation's own row speaks for the mails
  that are left — `thread_of_shown` — and the reading pane shows those same
  mails, since `open_conversation` reads the derived threads), a label filter's
  matches and a search's hits. The search popup's own "Only" boxes are the other
  layer: those narrow the *query*, in the database, and a query filter and a
  view filter together are an AND.
  **The predicates are one function over four facts** (`ListFilter::matches` over
  `FilterFacts`), because all three rows can answer them: a folder row from
  `MailMessage`, a hit from `SearchHit`, a label row from `LabelledMail` — which
  is why both of the latter carry the `To` header. "To me" is a substring test
  against the accounts' own addresses (`KimoApp::my_addresses`: each account's
  username *when it is an email address*, the rule the compose applies to what a
  mail is sent as), so an account whose username is a bare login leaves the
  question unanswerable and the menu row says so rather than filtering to
  nothing. **The derived rows carry the set they were built with**
  (`ListRows::filters`, beside `threaded` and the revisions), so a change derives
  them again and a frame that changed nothing derives nothing; the hits and the
  label matches are filtered as they are drawn, since neither is cached for the
  frame. A view that hides every mail leaves the column saying which kind of
  empty it is — "No mail matches this filter." against the folder's own "This
  folder is empty." and a search's "No hit matches this filter." — and
  **select-all takes what the list shows** (`ListRows::shown_indices`), never
  mail the filter has hidden.

- **The list sort is two independent choices, in one menu.** The title bar's
  sort cell (`sort_popup`) holds the *order* — "Newest on top" (the default, the
  order the folder arrives in) and "Oldest on top" — and the *key* — date,
  sender, subject — as two groups under their own small headings, each a choice
  of one drawn as a row with the tick at its end (`menu_row`, the same row the
  filter menu uses). The two are one comparison
  (`ListSort::compare`): the key decides, the order group turns it round, and
  where the mail sits is the tie-break — which is what keeps two mails equal by
  the key in the folder's order instead of swapping between frames, and what
  makes the default a no-op on the list as it already is. The keys are what a
  row *shows* (`SortKey`): a conversation's row is sorted by its newest mail's
  date and sender and by its own subject, and names are compared
  case-insensitively the way a reader would.
  **It is a view of the column like the filter**, and the two share a cell's
  worth of state: the derived rows carry both (`ListRows::filters` and
  `ListRows::sort`), so a change derives them again, the flat list is drawn in the
  order `flat_order` worked out rather than reordering the folder's own slice,
  and the label view's matches are sorted in their derivation (whose key carries
  the sort too) with its own total order behind them, since a label spans every
  folder and account at once. **A search's hits keep the query's order**: they
  are ranked, and relevance is what a search is for — the sort is about the
  lists that are the mailbox itself.
