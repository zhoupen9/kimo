# Accepted limitations and roadmap

Do not "fix" these casually — each is a known, decided state. Read before changing behaviour that looks wrong.

## Accepted limitations / roadmap

Do not "fix" these casually — they are known:

- Downloading an attachment is a round trip to the server, never a read of the
  cache: Kimo keeps the name, the MIME type and the decoded size and never the
  bytes, so a card's link fetches the mail's original source again and there is
  nothing to hand over for mail the server no longer has. Nothing is opened
  afterwards either — the file lands in `~/Downloads` (a download does not ask
  where, unlike Export) and the desktop's own programs are what open it. Export
  remains the way to get the whole message, attachments included, as one `.eml`.

- Threading is per folder and, for mail cached before schema v7 — or mail whose
  sender wrote no ids at all — by subject: a reply joins the subject it answers,
  but a conversation whose mails were filed into different folders (or into and
  out of a Trash) is more than one conversation here. Search results are a flat
  list of hits and are not grouped at all, and the reading pane shows a
  conversation only while the list is threading: with the preference off, a
  click opens the one mail it always did.

- **niri ignores `xdg_toplevel.set_minimized`, and Wayland has no unmap
  request**: with Preferences → General's "Minimize to the tray" off, the
  minimize button cannot minimize on niri. Turning that option on is the way
  to hide the window with the minimize button; "Close to the tray" gives the
  close button the same hide. Both work by dropping and reopening the toplevel
  (see [preferences.md](preferences.md)).

- **Gmail needs ports 993 and 465.** The Gmail module is IMAP and SMTP over
  TLS, not an HTTPS mailbox API, and there is no proxy setting in Kimo and no
  Gmail-over-443 provider to fall back to. On a network that filters those
  ports — the TCP connection is made, then dropped without a byte, which reads
  as `network: imap.gmail.com:993 closed before the greeting…` — Gmail cannot
  sync, while the OAuth sign-in (HTTPS) still works. That asymmetry is the
  network's, not the module's; a Gmail API module over HTTPS would be the way
  around it.

- TLS finite-field DH covers the 2048-bit groups listed in
  `src/protocol/ffdhe.rs` and nothing else. A server whose DHE group is
  another size, or yet another custom prime, still fails the handshake with
  `SelectedUnofferedKxGroup`; see the finite-field DH section in
  [protocols.md](protocols.md) for what to add.

- Deleting is permanent: there is no Trash folder and no undo, and a click on
  the title bar's Delete expunges the mail on the server without asking first —
  the one the reading pane shows, or a whole select-mode selection, which is
  just as final. There is no Delete-key shortcut and no shift-click range: a
  selection is made one checkbox at a time, or every mail of the list at once
  (see the select-mode and message-row bullets in [ui-mail.md](ui-mail.md)) — and
  that click is just as final, as is a row's own hover buttons.

- Labels are **local to this machine**: nothing about them is asked of or told
  to a server, so another client's labels are invisible here and Kimo's are in
  `mail.db` alone — a label names a message row, so mail that comes back under
  a new UIDVALIDITY generation comes back unlabelled. A label cannot be renamed
  or deleted (the popup only puts one on a mail or takes it off, and a name no
  mail carries any more stays in the column), nothing is labelled in bulk
  (select mode leaves Delete as its one live action, and the label cell on a
  mail's head is about that mail), and the filter is in memory only.

- **A mail can carry files, and the card is where they are chosen.** Two ways
  in, and one area that says both (`attach_card`): **a click opens the desktop's
  own dialog** (`crate::portal`: the XDG portal's
  `org.freedesktop.portal.FileChooser.OpenFile` over D-Bus, which runs the dialog
  in the *compositor's* process — nothing is linked in, and it is the one way
  that works on Wayland at all), and **a file dragged from a file manager** lands
  on the window (egui's `dropped_files`, read in `KimoApp::ui` rather than in the
  card: a drop belongs to the frame, and one that arrives with no card open
  *starts* a card with the file on it, which is what dropping a file on a mail
  client means). The area replaces the button, path box and Attach button that
  used to say the same thing — the dialog is the way in, and a typed path was
  only ever the fallback for a session with no dialog to open. What a file that
  cannot be attached has to say — a directory, something that has gone, a desktop
  with no portal — is drawn under the area, never beside it.
  ```
  Files  [ file card  ✕ ]  [ file card  ✕ ]
         [ Click to select files, or drag files here ]
  ```
  **A file is a card**, the reading pane's own (`attachment_card`, drawn here
  with a click that takes the file off the mail rather than fetching it, which is
  the one thing the tooltip's last line varies by), wrapped across the field
  column; the same file twice is two parts, because a mail may carry one file
  under two names. Each of those cards carries a
  **small round cross** in its right-hand gutter — the room for it is part of the
  card’s own width, so a long name is cut before it, never under it — and the
  cross is a widget of its own: the treatment the row delete button gets (muted
  until the pointer is on the button, then the scheme’s `danger_hover` fill with
  the glyph in `on_danger`), the tooltip "Take this file off the mail", and a hit
  box of its own beside the card’s body, which does the same thing and says so.
  The reading pane’s cards have **no** cross (`removable: false`): a mail’s own
  attachment is not Kimo’s to take away. On a Wayland session the drag half of the area’s sentence is a
  promise the platform does not keep: winit 0.30 has no drag-and-drop in its
  Wayland backend, so no `HoveredFile` or `DroppedFile` ever reaches the window
  and the card's "Drop the files here" state cannot appear. The dialog is
  therefore the way in that has to work.
  **The draft holds paths, not bytes** (`outgoing::OutgoingAttachment`: the path,
  the name, and the `Content-Type` its extension suggests, `content_type_of`), and
  the worker reads them as it sends (`load_attachments`): the UI thread reads no
  files at all (rule 3), and what goes out is the file as it is when the mail
  leaves rather than as it was when it was chosen. A file that cannot be read
  stops the send with its path in the reason — a mail that goes out without the
  file its writer attached is worse than one that does not go out — and the same
  failure is what the SMTP send and the Sent copy both see, because the message
  is rendered once (`send_message`) and the same bytes are delivered and filed.

- **A mail with files on it is `multipart/mixed`.** `outgoing::render` writes the
  headers it always did and then, instead of a bare `text/plain` body, a boundary
  and one part per thing: the letter first — so a client that shows only the
  first part of a multipart body still shows the letter — then a part per file,
  each with its `Content-Type`, `Content-Transfer-Encoding: base64` and a
  `Content-Disposition` that names it. The boundary carries an underscore
  (`=_kimo_<id>_=_`), and the base64 alphabet has none, so no encoded part can
  contain it — which is the one thing a boundary has to be able to say. A name
  that is not ASCII is written twice: a quoted form with the name reduced, which
  is what an old client shows, and RFC 2231's `filename*=utf-8''…`, which is what
  a client that knows about it reads. The files are read whole into memory —
  there is no streaming and no size cap — which is the price of the rest of the
  app being the same.

- A written mail goes out over SMTP and no further: no HTML body (the card is a
  text box, so the letter is `text/plain; charset=utf-8`, base64-encoded), no
  drafts — closing the card is the end of
  the mail, and a failed send keeps the draft in the card but nowhere else — no
  outbox, queue or retry, no DKIM, and no OAuth2: submission authenticates with
  `AUTH PLAIN`/`AUTH LOGIN` over TLS, or not at all when the server asks for no
  login. Every title-bar mail action is wired (reply, reply all and forward open
  the compose, label opens the label popup, flag marks the mail, export and
  delete hand it to a worker, and the compose pen is wired; the mail's own header
  carries the same cells minus flag and export and answers on the spot — see
  [compose.md](compose.md) and [ui-mail.md](ui-mail.md)).

- Credentials in plain text in `mail.db` (mode 0600, see [storage.md](storage.md));
  no
  OAuth2/XOAUTH2. Outlook and Microsoft 365 now require OAuth2, so the
  provider preset fills their endpoints and says in the form that Kimo cannot
  sign in to them; an account saved anyway fails at the login with the
  server's own words. The database is not encrypted, and the app never renames
  or deletes it on its own: a corrupt file is the user's to move aside.

- **The vendor is inferred from the account's hosts, not stored.** A server
  Kimo does not recognize is Custom — the old name heuristics and the account's
  own fields, unchanged — so a Google Workspace or Exchange account behind a
  custom host is Custom too. There is no per-account override. A vendor preset
  switched off in Preferences → Accounts is simply Custom everywhere (no
  preset, no Sent-mailbox preference); the account keeps syncing over the
  standard protocol. See [providers.md](providers.md) for why the column was
  rejected.

- HTML mail renders through the in-egui subset renderer (`src/html.rs`).
  CSS is the `src/css.rs` subset (colors, fonts, sizes, display/visibility);
  compound selectors and layout CSS are ignored. Simple tables render as
  aligned grids; layout tables (block elements inside cells — most HTML
  mail) render as flowing full-width rows, because egui Grid sizes columns
  to the minimum wrapped width. `cid:` inline images render from the
  attachment bytes stored on the message (`InlineImage.data`, the
  `inline_images` table). Improving fidelity here means improving this renderer — Blitz mode
  is the escape hatch for mail this subset cannot lay out, not a replacement.
  **A mail's colours are adapted to the theme** (`html::adapt_color`), which is
  what makes this renderer theme-following rather than theme-blind: mail is
  written for white paper, and the dark theme draws it on a surface that is
  nothing like it — the sender's `#000000` on the card is 1.4:1, which is not
  text. A colour a declaration sets is read as CSS and moved through OKLCH, so
  what changes is lightness and not identity: the **light** theme draws every
  colour as written; in the **dark** theme a background above 0.85 lightness
  becomes the surface mail sits on (`0.12, 0.12, 0.14`), text below 0.25
  becomes (`0.90, 0.90, 0.92`), text above 0.85 is left alone, and the middle
  keeps its hue and chroma with its lightness flipped. It happens in
  `apply_decls`, once per declaration, so what a wrapper sets is inherited down
  the tree already adapted, and the alpha a colour carries is kept.
  **The flip is unconditional**, so a mid-tone above half lightness comes back
  *darker* than it went in: `color: red` (`#FF0000`, l 0.63) is drawn
  `#9F0000`, which on the dark card is less readable than the red was — lifting
  instead of flipping is a one-line change if the owner wants it. **Blitz
  transforms nothing at all** — it draws the sender's page, on a paper of its own
  (see the paper bullet below).

- Blitz renders tables in the *separate* borders model: `blitz_render`'s
  `COMPAT_CSS` overrides `border-collapse` because blitz's collapsing paints
  the first cell's border width/colour across the whole table — a 3px dark
  frame around every `border-collapse: collapse` table whose first cell
  declares no border, and wrong-coloured cell borders. Consequence: adjacent
  cell borders do not merge (a 1px border on both sides of a shared edge draws
  2px), and `cellspacing` stays 0 unless the mail sets `border-spacing` in
  CSS. Fixing this properly means fixing blitz, not this workaround.

- **A Blitz page is drawn on a paper** (`draw_blitz_page`, the pane's own
  doing — nothing in the render path is touched): the mail's colours are never
  transformed, but a mail that declares no background of its own would otherwise
  be composited straight onto the pane's card, and in the dark theme that is the
  sender's `#000000` on `#24272E`, 1.4:1. So the pane fills the page box with a
  paper before it blits the raster: **white** by default, and switchable per mail
  from the contrast cell at the inner end of the mail head's action run (the
  sixth cell, `header_action_center(5)`; `Paper`, `KimoApp::blitz_paper`,
  `toggle_blitz_paper`). White is the default and is *not* stored, so the map
  holds exactly the mails the reader changed, and only for the session — nothing
  goes to `settings.json`. A mail that **does** declare a background keeps it:
  `blitz-paint`'s `paint_scene` resolves the root element's background, else
  `body`'s, and fills the whole page with it, so the raster is opaque across the
  page and the paper behind it never shows. That is also why the cell is not
  hidden for such a mail — nothing in Kimo reads the mail's CSS to decide.
  `BLITZ_PAPER_DARK` is the dark scheme's own `content` surface, and the paper
  is deliberately not part of `blitz_key`: flipping one repaints a frame and
  starts no render. The cell is drawn only where a Blitz page is (`html_renderer`
  is Blitz and the mail has an HTML body), so Simple mode and plain-text mail
  have none.

- Blitz mode is a raster: text is not selectable, links are clickable boxes
  (from `get_client_bounding_rect`), form controls are inert, and mail with a
  fixed-width layout is laid out at its own width and scaled down into the
  pane. The finished page is recorded once as an anyrender `Scene` and
  rasterized one 2048 px slice at a time, so page height is not limited by a
  single surface (`Scene` + `append_scene` with a translation; see
  `blitz_render::render`). A page beyond the 40 MP budget (160 MB of textures)
  is re-laid out at a smaller scale — see the paint-scale rule in
  [html-mail.md](html-mail.md) —
  and only a page too tall even for the 0.5× floor is cut, which sets
  `Page::truncated` and shows a note in the pane. The reading pane also
  scrolls back to the top whenever another message is opened. Remote
  resources are fetched during the render, so the first view of an
  image-heavy mail can take a few seconds (the disk cache makes the second
  one fast). Fonts are the curated disk set, not fontconfig's.

- POP3 changes one thing server-side — the `DELE` behind the Delete button —
  and a sync does not notice deletions either: mail removed in another client stays in the database
  and in the list forever. What a sync does is incremental by UIDL — a message
  already stored is never downloaded again — and bounded below by the
  `first_num` floor, so the messages a first sync skipped arrive only when
  `Load older mail` asks for them.

- **The GPU pick is per Vulkan driver, and only PCI display controllers are
  offered.** An ICD manifest names a library, not a card, so two cards of one
  vendor share the one driver that is loaded; the pick then still chooses the
  card for rendering (by PCI bus id), but cannot keep the other card's driver
  out. A GPU that is not a PCI display controller — an ARM platform device, for
  one — has no `PCI_ID` in sysfs and is not listed, leaving Automatic as the
  only choice there. A pick that no longer resolves (the card unplugged, a
  `settings.json` from another machine) falls back to every driver, and only
  the log says which value was ignored.

- Flags of mail that is already stored are not read back from the server. IMAP
  `\Seen` **and** `\Flagged` are taken for a message as it is fetched — the two
  states `fetched_state` reads off the `FETCH`, and the flag is kept for a row the
  database is storing for the first time — and from then on the local state is
  the only truth: `seen` only ever grows, and `flagged` is not named by
  `MESSAGE_UPSERT`'s update at all, so a re-fetch leaves the reader's own click
  where it was. Sync used to refresh the newest batch for free by refetching it;
  now that a warm sync fetches nothing it already has, mail read — or flagged —
  in another client keeps the mark it had here.

- No IDLE/push for IMAP; sync is manual or on-startup. A desktop
  notification is therefore only ever raised by a sync that ran, and never for
  mail that arrived while the app was closed until the next one asks for it:
  `Job::Sync` at startup, the title-bar button, the automatic sync, or the
  first click on a folder. A notification does nothing when it is clicked —
  the default action would mean listening for the daemon's `ActionInvoked`
  signal, which this client does not do — and its icon is scaled for the
  monitor the window is on, so a notification a multi-monitor daemon draws on
  another screen keeps that scale.
