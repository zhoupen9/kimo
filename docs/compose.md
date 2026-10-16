# Compose — the card and the mail that goes out

Read when touching `compose_card`, `src/outgoing.rs`, or `Job::SendMessage`.

- **Sending is SMTP submission.** The compose card's Send hands its draft to
  a worker (`Job::SendMessage`), which speaks `EHLO`/`STARTTLS`/`AUTH`/`MAIL
  FROM`/`RCPT TO`/`DATA` through `src/protocol/smtp.rs`, and the message itself
  is built by `src/outgoing.rs`. A copy goes into the account's Sent mailbox
  over IMAP when the server has one. A mail with files on it goes out as
  `multipart/mixed` — the text is one part and each file another, all base64 —
  and the files are read by the worker as it sends, never by the UI thread. There
  is no drafts folder and no outbox — see hard rule 4 in AGENTS.md and the
  compose card below.

## Sending is a job

- Sending is a job like any other, with two differences. `Job::SendMessage`
  renders the draft (`outgoing::render`), connects, authenticates when the
  server advertises a mechanism this client speaks, and hands the message to
  `DATA`; the outcome comes back as `Event::MessageSent { recipients, copy }` or
  `Event::MessageSendFailed { error }`. The first difference: it is *not*
  bracketed with `SyncStarted`/`SyncDone` — the compose card that asked is on
  screen, cannot be closed while the mail is in flight, and says `Sending…` —
  and a failure is the card's error, not the account row's warning. The second:
  `Event::MessageSent` is what closes the card, because until the server says it
  has the mail the draft is the only copy of it. `SentCopy` is what became of
  the copy in Sent: `None` for a POP3 account or a server that lists no Sent
  mailbox (`folders::kind_of` is the table that recognizes `Sent`, `Sent
  Items` or 已发送邮件, whatever wire form the name arrived in), `Saved {
  mailbox }` after an `APPEND` and the incremental fetch of that folder on the
  same connection — so what was sent is in Kimo's Sent when the user opens it,
  not at the next sync — or `Failed`, which un-sends nothing and is what the
  notice says.

## The compose card

- The compose card comes in two containers (`ComposeSpot`) and is one card
  (`compose_card`). The title bar's pen — and its reply, reply all and
  forward — bring it up as the `egui::Modal` it always was (`compose_window`):
  a card `COMPOSE_WIDTH` wide in the foreground layer, whose backdrop dims the
  columns and the bar and takes their clicks, so a mail is written with the
  rest of the app out of the way. The reply, reply all, forward and delete
  cells on a mail's own header (`mail_sheet` — the title bar's own cells and
  glyphs, at the right end of the card's head row, delete outermost) open the same
  three *on the spot*: one more card of the reading pane, right under the
  mail answered (`compose_inline_card` — the pane's own surface at the pane's
  width, a card gap above it), and the mail stays on screen above its answer
  until the card is done — sent or discarded, or, in the popup layout, backed
  out of by the floating back button, which closes the card with the view it
  was opened in (a draft whose Send is in flight is owed its event, and the
  button waits for it). The card answers the mail whose header was clicked —
  in a conversation that need not be the mail the pane was opened on — and
  `compose_anchor` holds which mail that is; a draft whose mail has left the
  pane (another folder opened, the mail deleted) moves to the popup window
  rather than sitting under mail it was never about (`compose_anchor_on_pane`,
  from `ui`). There is one draft at a time — `KimoApp::compose` holds it —
  and the pen leaves an open card alone rather than resetting what has been
  typed into it; the three answers do not (the click is the user's asking for
  another draft), except a draft already on its way out, which no click
  replaces: its Send is owed an event.
  **The three that answer a mail are made of it** (`ComposeDraft::reply`,
  `reply_all`, `forward`): a reply is addressed to the sender alone, a reply
  all to the sender and every other recipient — the account's own address
  excluded, so nobody is sent their own mail — and a forward to nobody until
  the sender chooses. All three quote the original under the new text
  (`quoted_body`: who wrote it and when, their text a `>` line at a time —
  the text part, or the HTML rendered down to words, because the card is
  plain text), and prefix the subject (`answer_subject`) unless it already
  carries a prefix, whatever prefix the sender's client wrote
  (`threads::strip_subject_prefix`'s table, the threading's own).
  From is the configured accounts, defaulting to the one whose mail is open and
  falling back to the first (`compose_from`); To, Cc, Bcc, Subject and the body
  are plain strings in `ComposeDraft`, deliberately not widget state: what a
  card opens with is the draft's own — nothing, for a new mail; the mail it
  answers, for the three. **The on-the-spot card is the shorter form**
  (`compose_card`’s `inline`, which is what `ComposeSpot::Pane` means): an
  answer written under its own mail shows From, To, the body, the files and the
  two buttons, and nothing else — in that order, a letter’s: the mail is what
  the card is for, and a file is something put *with* it (`compose_body` before
  `compose_files_row`) — and the popup draws the same card under its heading,
  its Subject row the one thing the answer on the spot has no use for. The
  attachments are the card’s own row in either container: no label beside them,
  and the cards and the box across the card’s whole width (`compose_files_row`).
  The answer on the spot has no heading — the mail above it says what it answers
  — but both containers have a **head**, in two columns: the
  avatar of the account the mail goes from on the left, and the lines it goes out
  on beside it — From first, then To and the copies its chevron folds out
  (`to_and_copies`), and on the popup’s form Subject after them. The rows wear a
  mail header’s labels, colons and all (`From:`, `To:`, `Cc:`, `Bcc:`,
  `Subject:`). No rule under the head in either container: the popup’s heading is
  what sits over its form, and the answer under its mail is a letter’s own first
  lines.
  The list is *flat* in either container: `flatten_widgets` takes the combo’s
  outline and fill off in that Ui and leaves the hover, because an outline
  belongs to a form of boxes and this line is the card’s own from. **The flat
  list ends with the To row’s chevron** rather than the filled triangle a combo
  box carries (`paint_from_chevron`: the To row’s own drawing, painted in the
  place egui would put its triangle, down while the list is away and up while it
  is open — the head of a letter ends the way the row under it does, weak until
  the pointer finds it). **To is
  flat beside it**, for the same reason: the row’s field is added with
  `Field::flat`, a field whose frame draws nothing — so it has no box focused
  or not — which is also what a flat *field* wants where `take_box_off` would
  do: the visual it clears is the colour of the text selected inside the
  field, and a flat field has no business recolouring that. The popup’s own
  fields are `Field::quiet` instead: To, Cc, Bcc and Subject draw the input
  fill with no line until the pointer or the keyboard is on one
  (`docs/ui-style.md`). The face is the one a mail’s
  card wears (`sender_color` and `sender_initial` of the
  account’s address and name, at `AVATAR_DIAMETER`), so the card says who it is
  from the way every other card in the app says who it is from. **To ends with
  the fold chevron** (`paint_fold_chevron`, in a cell a field tall): the Cc
  and Bcc lines are one click away under To (`compose_to_row`) in either
  container — flat like the lines above them on the spot, quiet in the popup’s
  form, pointing down while they are away and up once they are under the row —
  and a card opens with them away (`ComposeDraft::copies_open`), so a card that
  needs no copies shows the one address it goes to. A Reply all’s own Cc is
  still *in the draft*, and read — and edited — as soon as the chevron is
  clicked. Subject is the popup’s own row — quiet like the lines above it, and
  always drawn — and it stays in the draft with them. The popup keeps its
  heading and its Subject row, and one draft moves between the two containers
  with only the
  container changing.
  The footer is the card’s own in either container: `compose_button`’s pair — an
  icon and a word in one tall target, Send in the accent because sending is what
  the card is for (`paint_send_icon`, the plane) and Discard an outline beside it
  that turns the scheme’s danger on hover (`paint_delete_icon`, the trash, the
  treatment every delete in the app gets). **The word follows that ink** — white
  on the danger fill, greyed with the rest of the disabled treatment — because a
  galley carries the colour it was laid out in and the shape has to override it
  (`compose_button`). **The card draws no rule at all in either container** —
  neither under its head nor over its foot, and the popup’s heading above its
  form carries none either: the card is a letter, and the mail above it — not a
  line — is what its ends are read against. What the card has to say (the
  server’s refusal, that the mail is on its way, why Send cannot be pressed)
  stands on a row of its own over the buttons, and the pair opens the row at the
  card’s left edge — the side the mail above is read from — rather than ending
  it at the right: **Discard first and Send beside it**. That row is a *bounded*
  one (`ui.horizontal`, the height of what is in it): a `with_layout` over the
  space left would center the pair in all of it and carry the card’s surface
  down to the pane’s floor, which is what left a band of nothing between the
  reason and the buttons — **the card ends where its content does**,
  `CONTENT_PADDING` under the last row on it. The boxes are a fixed label column
  plus what `COMPOSE_WIDTH` leaves, behind the head’s avatar column; the body is
  `COMPOSE_BODY_HEIGHT` tall and scrolls inside that rather than growing the
  card — and **the box fills that height and the card’s whole column**
  (`compose_body`’s `min_size` and the width the card hands it), not the dozen
  lines an empty draft
  needs — so the row under it is never read across a band of the card’s own
  surface and the mail being written is as wide as the card will hold. **The
  box is the card’s, not the text’s** (`compose_body`): a text edit draws its
  own frame about what it holds, and an answer holds a quoted mail — more
  text than the `COMPOSE_BODY_HEIGHT` viewport holds — so a frame of the
  field’s own would run past the room at both ends and go where the scroll
  goes: its top out of sight after a scroll down, its bottom out of sight
  before one, which reads as a box with two sides. The fill and the line are
  painted here instead, in the viewport’s own rect, the line answering the
  keyboard the way every field’s frame does (`Visuals::selection`), and the
  field is given no frame of its own (`take_box_off`). **The
  area is claimed outright** (`compose_body`’s `scope_builder` over a rect of
  its own), because a window too short for the card is the pane’s to scroll:
  a scroll area left to the room under the To row is squeezed to a few lines
  there, which cuts the box off and leaves the card’s surface not even
  covering what is left of it. The
  first frame puts the keyboard where the card’s own next move is: To on the
  popup’s form — and on a forward opened on the spot, which has no recipient
  yet — and the body on an answer that already knows who it goes to, with
  `cursor_at_end(false)` so the caret starts the reply *above* the
  quoted original rather than after it. That same first frame is what brings an
  on-the-spot card into view: `compose_inline_card` reads the draft’s `focus`
  before `compose_card` takes it and scrolls the pane by no more than the card
  needs (`Ui::scroll_to_rect(.., None)` — the shortest scroll that shows it,
  and its head when it cannot all fit), because a reply cell at the top of a
  long mail would otherwise open its card below the fold with nothing on
  screen to say the click had done anything.
  **The backdrop does not close it.** `ModalResponse::should_close` — a click
  beside the card, or Escape — is deliberately ignored: the draft is the only
  copy of the mail, and neither should be what throws it away. Discard is the
  way out. The app's other two modals do close on the backdrop; the difference
  here is the draft.
  **Send is wired, and the card owns everything about it.** One rule decides
  whether the mail can go out — `send_blocker`: an account, one that has an
  outgoing server, a username that is an address, at least one recipient, and
  every entry in To/Cc/Bcc an address (`outgoing::parse_addresses` is what
  unwraps a `Name <addr>` and refuses a typed CR/LF before it could become a
  header of its own). The footer shows that reason, the `Sending…` state while
  a worker has the mail, or the server's own refusal — and the worker checks
  the same rules again in `sync::send_message`, because nothing leaves the
  machine as mail on the strength of a frame having said it could. While a send
  is in flight the whole card is disabled, Discard included: a draft that is
  already on its way out is not one to edit or drop.
  **Nothing is persisted.** There is no drafts folder and no outbox, so Discard
  is the end of the mail — and until `Event::MessageSent` arrives, the card is
  the only copy of it, which is exactly why that event is what closes it. A
  refusal keeps the card (and the draft) with the server's words in it, ready
  to be sent again. Adding a way to keep a draft, attach a file or queue a send
  is a decision for the project owner, not an oversight to fix in passing.
