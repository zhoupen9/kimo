# UI style — the rules a drawn widget obeys

Read before drawing a control, an icon, a row that opens, or a mail field.
These are the house rules for how Kimo's widgets look: they outrank the look
egui hands a stock widget, and they are rules rather than descriptions — where
the code still differs, the gap is named under the rule and the per-area doc
([compose.md](compose.md), [ui-mail.md](ui-mail.md)) describes the code as it
stands.

The colors these rules name are [theme.md](theme.md)'s; the compose card the
fields and the fold belong to is [compose.md](compose.md)'s area, and the
list's own chevron is [ui-mail.md](ui-mail.md)'s.

## A disclosure is a chevron, never a solid triangle

**The rule.** Anything that opens, closes, folds or unfolds is drawn as two
strokes meeting at a point — a chevron — never as a filled triangle. A solid
triangle is the one shape that reads as a stock widget rather than as part of
the chrome. The app already has the two drawings a disclosure needs; a widget
uses one of them rather than inventing a third:

- `paint_chevron(painter, centre, size, open, color)` — the message list's own
  disclosure: pointing right while the thread's mails are hidden, down while
  they are under it (`THREAD_CHEVRON`, drawn at `THREAD_CHEVRON_ALPHA` of
  `scheme.text_weak` at rest).
- `paint_fold_chevron(painter, centre, size, open, color)` — the same drawing as
  a fold: down while what it opens is away, up while it is out
  (`COMPOSE_FOLD_GLYPH`). `paint_combo_chevron` is that chevron where an
  `egui::ComboBox` would put its triangle.

The color is the caller's, from the scheme or from the place the glyph sits
(`scheme.text_weak`, `card_action_color`) — never egui's default widget color —
and a chevron that is clickable is a hover target: `PointingHand`, and hover
text saying what the click does.

**Where the code stands.** The message list's conversations, the popup pane's
back button, the compose card's To row and From list in either container, the
search popup's scope, and the navigation column's account headers all draw
chevrons. One thing does not:

- egui's own `ComboBox` triangle (Preferences' font picker) is egui's internal
  drawing and stays: a form's box keeps its box's glyph. The combos that are a
  card's own lines rather than a form's boxes — the compose card's From list and
  the search popup's scope — are drawn flat, and `paint_combo_chevron` paints
  over the place their triangles would take.

## A mail address field has no outline until the pointer or the keyboard is on it

**The rule.** From, To, Cc and Bcc — the compose card's address lines, in either
container — draw no outline at rest: no line that boxes them in. While the
pointer is over one, or the keyboard is in it, it wears one line in the reader's
accent (`scheme.accent`), `FIELD_STROKE` wide, drawn inside the field's own
rect, in the corner radius of its state. Hover and focus are the same accent:
the box appears with the first of them, and goes when neither is true. The rule
is about the line, not the fill — a field that already has a fill keeps it.

The box is Kimo's own, painted in `Field` over a frame that draws nothing
(`field_frame`), never egui's: egui's slot for a focused field's line is
`Visuals::selection.stroke`, the selected-*text* color, so a field that let
egui draw it would recolour every selection in the app. The accent is a fill and
focus color, not text — [theme.md](theme.md)'s contrast rule says so.

**The text's margin is the frame's.** A field given a frame of its own — which is
every field here — ignores `TextEdit::margin`: egui builds a frame out of that
number only when the caller hands it none, and `Field` hands it `field_frame`.
Where a glyph is drawn inside the box, the room for it is `Field::margin` — the
search bar's magnifier and clear cross (`SEARCH_BAR_MARGIN`) — and that is what
keeps the query and its hint clear of the icon.

**Where the code stands.** The compose form follows the rule: its address lines
are `Field::quiet`, and Subject is one of them so the form reads as one thing
rather than three quiet rows between two boxed ones — and so do the search
popup's filter boxes, From and To among them, which are address lines by name.
The compose card's From list and the search popup's scope are neither: they are
combos rather than `Field`s, and they are drawn as a card's own lines —
`flatten_widgets` takes egui's outline and fill off them and leaves the hover
fill that says they open, with the chevron of rule 1 where egui's triangle would
be. `Field::flat` — the on-the-spot card's lines — is still the
other state: it draws nothing at all, focused or not, so the answer under its
mail has no box to bring out. The body is not one of these fields — it is the letter's
writing area, whose box is the room the card gives it (`compose_body`) — and the
two search bars are the bars' own boxes, not address lines.

## Cc and Bcc are folded away until they are asked for

**The rule.** A compose card draws To alone. Cc and Bcc are not displayed by
default: the To row ends with a chevron button that folds them out under it and
folds them away again — down while they are away, up while they are out — and a
click toggles the two rows. The button is a target of its own at the end of the
To field, a `COMPOSE_FOLD_CELL` (a field-tall) square, and it says what it does
on hover: "Add Cc and Bcc" while folded, "Fold Cc and Bcc away" while out.

The fold is display only. What is in the draft stays in the draft — a Reply
all's own Cc is there to read the moment the chevron opens it — and sending
reads the draft, never the screen.

**Where the code stands.** `compose_to_row` and `ComposeDraft::copies_open` are
the whole of it, and both containers draw the row: the on-the-spot card flat,
the popup's form quiet. `the_replys_to_row_folds_cc_and_bcc_out` pins the
behaviour on the spot, and `a_mails_header_answers_it_where_the_mail_is` the
popup's — folded on the first frame, out again from the chevron's click.

## A context menu is flat rows at the pointer

**The rule.** A right-click menu is a menu, not a form: flat full-width rows,
one action each, the scheme's hover under the pointer, and a destructive row
in the scheme's danger color (turning to `on_danger` on its danger fill).
There are no stock buttons, no checkboxes and no icons in one unless the
action already has the app's glyph; the whole row is the target. The menu
opens at the pointer with `Popup::context_menu` when a widget's response is
what opened it, or as a `PopupKind::Menu` at a remembered position when the
things it belongs to are drawn before the menu can be (the mail body). Escape
and an outside click close it, as with every other menu.

**Where the code stands.** `context_menu_item` is the row. A message row's
menu is `mail_context_menu`; the reading pane's is `body_context_menu` (its
rows keep the label selection alive, see [ui-mail.md](ui-mail.md)); a Blitz
page's link boxes draw theirs in `draw_blitz_page`.

## Working by these rules

- Draw the shape and take the color from the scheme or from the glyph's own
  place; do not leave an egui default in because it is close enough.
- When you are already in a widget that does not follow a rule, bring it in line
  and say so in the commit.
- The layout has no golden tests: run `cargo run -p kimo` and use the widget; the
  headless dumps and what they can and cannot show are in
  [testing.md](testing.md).
