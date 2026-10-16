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
  (`COMPOSE_FOLD_GLYPH`). `paint_from_chevron` is that chevron where an
  `egui::ComboBox` would put its triangle.

The color is the caller's, from the scheme or from the place the glyph sits
(`scheme.text_weak`, `card_action_color`) — never egui's default widget color —
and a chevron that is clickable is a hover target: `PointingHand`, and hover
text saying what the click does.

**Where the code stands.** The message list's conversations, the popup pane's
back button, and the compose card's To row and From list — in either container —
all draw chevrons. Two things do not:

- `account_row` — the navigation column's account headers — still fills a
  `convex_polygon` triangle. It is the one disclosure in the app that is not a
  chevron; it becomes `paint_chevron` when that row is next touched.
- egui's own `ComboBox` triangle (the search-scope menu, Preferences' font
  picker) is egui's internal drawing and stays: a form's box keeps its box's
  glyph. The compose card's From list is the one combo that is a letter's line
  rather than a form's box, so `paint_from_chevron` paints over the place its
  triangle would take.

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

**Where the code stands.** The popup's form follows the rule: its address lines
are `Field::quiet`, and Subject is one of them so the form reads as one thing
rather than three quiet rows between two boxed ones. The From list is neither: it is a combo
rather than a `Field`, and the card draws it as a letter's line in either
container — `flatten_widgets` takes egui's outline and fill off it and leaves
the hover fill that says it opens, with the chevron of rule 1 where egui's
triangle would be. `Field::flat` — the on-the-spot card's lines — is still the
other state: it draws nothing at all, focused or not, so the answer under its
mail has no box to bring out. The body is not one of these fields — it is the letter's
writing area, whose box is the room the card gives it (`compose_body`) — and the
title bar's search box is the bar's own box, not an address line.

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

## Working by these rules

- Draw the shape and take the color from the scheme or from the glyph's own
  place; do not leave an egui default in because it is close enough.
- When you are already in a widget that does not follow a rule, bring it in line
  and say so in the commit.
- The layout has no golden tests: run `cargo run` and use the widget; the
  headless dumps and what they can and cannot show are in
  [testing.md](testing.md).
