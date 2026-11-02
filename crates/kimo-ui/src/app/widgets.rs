//! Shared custom widgets and row primitives: field frames, flattened
//! controls, one-line elision, attachment cards and label chips.

/// The labels a mail carries, drawn on its own card under the head: one chip per
/// label, as wide as the name wants, wrapping when the run does not fit the
/// card. The fill is the scheme's hover — the palette's faintest surface that is
/// not the card itself — and the text weak, because the labels are a note about
/// the mail rather than part of it. The corner is half the height, so a chip
/// reads as a pill rather than as a small button.
pub(super) const LABEL_CHIP_HEIGHT: f32 = 18.0;
pub(super) const LABEL_CHIP_PADDING: f32 = 6.0;
pub(super) const LABEL_CHIP_GAP: f32 = 4.0;
pub(super) const LABEL_CHIP_CORNER_RADIUS: u8 = 9;
/// The attachment cards under a mail's body: one card per file, as wide as the
/// name and the type it carries want — never narrower than `ATTACHMENT_CARD_MIN_WIDTH`
/// and never wider than the mail's own column — and the height of a line of the
/// list. They are laid across that column and wrapped onto the next row when the
/// next card does not fit, `ATTACHMENT_CARD_GAP` between two of them either
/// way, and `ATTACHMENT_CARDS_TOP_GAP` between the run and the body above it.
/// The card's own paddings are the glyph's inset, its size, the gap it leaves
/// the text and the text's own right inset; only a name the column itself cannot
/// hold is cut, and the tooltip then shows it.
pub(super) const ATTACHMENT_CARD_MIN_WIDTH: f32 = 140.0;
pub(super) const ATTACHMENT_CARD_HEIGHT: f32 = 52.0;
/// The card's two lines, in points: the file name and, under it, the type and
/// the size. Both are smaller than the body text — a card is a note about the
/// mail, not part of it.
pub(super) const ATTACHMENT_NAME_FONT_SIZE: f32 = 11.0;
pub(super) const ATTACHMENT_META_FONT_SIZE: f32 = 9.0;
pub(super) const ATTACHMENT_CARD_GAP: f32 = 8.0;
pub(super) const ATTACHMENT_CARDS_TOP_GAP: f32 = 18.0;
pub(super) const ATTACHMENT_CARD_CORNER_RADIUS: u8 = 6;
/// The cross a compose card's file is taken off with: a circle of this size, its
/// own corner radius (half of it — a circle), and the round cross glyph inside.
pub(super) const ATTACHMENT_CLOSE_SIZE: f32 = 18.0;
pub(super) const ATTACHMENT_CLOSE_RADIUS: u8 = 9;
pub(super) const ATTACHMENT_CARD_PADDING: f32 = 12.0;
pub(super) const ATTACHMENT_GLYPH: f32 = 22.0;
pub(super) const ATTACHMENT_LINE_GAP: f32 = 2.0;
/// Lay out a single line of text, cut with an ellipsis to fit `max_width`
/// pixels.
///
/// The cut is epaint's own — one layout job whose `TextWrapping` ends the row
/// at `max_width` and puts the ellipsis there (`max_rows: 1`,
/// `overflow_character`) — rather than a binary search over cut prefixes. A
/// search lays a galley out per probe to measure it: up to `log2(chars)`
/// shapings for one line, every probe left in the galley cache, and its cost at
/// its worst exactly when the width has moved far enough that all of the probes
/// are new — the first frames of a slide or a splitter drag. Here a line costs
/// one shaping whatever the width did, which is what the callers need: the
/// message list lays out every visible row on every frame the column's width
/// moves.
pub(super) fn truncate_galley(
    ctx: &egui::Context,
    text: &str,
    size: f32,
    color: egui::Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_format(
        text.to_owned(),
        egui::text::TextFormat::simple(egui::FontId::proportional(size), color),
    );
    job.wrap.max_width = max_width;
    job.wrap.max_rows = 1;
    // What `TextWrapping` asks for when `max_rows` is 1: without it a
    // run with no break in it (an address, a CJK subject) would not be cut where
    // the row ends.
    job.wrap.break_anywhere = true;
    ctx.fonts_mut(|f| f.layout_job(job))
}
/// Take the box off the widgets of this `Ui`: no outline, no fill, so what is
/// drawn in it stands on the surface under it and the box — if the caller
/// wants one — is the caller's to paint.
///
/// A text box wears its box from two slots of its own rather than the widgets'
/// — the fill it sits on (`Visuals::text_edit_bg_color`) and, with the
/// keyboard in it, the stroke of the selection — so both are taken off here
/// too: a field whose box is drawn by hand is flat focused and unfocused
/// alike. A combo reads neither, so the From list loses nothing by it.
///
/// It is scoped to the `Ui` it is called on, so the rest of the form keeps its
/// boxes.
pub(super) fn take_box_off(ui: &mut egui::Ui) {
    let visuals = ui.visuals_mut();
    visuals.text_edit_bg_color = Some(egui::Color32::TRANSPARENT);
    visuals.selection.stroke = egui::Stroke::NONE;
    let widgets = &mut visuals.widgets;
    for state in [
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        state.bg_stroke = egui::Stroke::NONE;
        state.weak_bg_fill = egui::Color32::TRANSPARENT;
    }
}
/// `take_box_off`, and the hover a flat control keeps: one drawn as a line of
/// text rather than as a box still says it can be used by filling under the
/// pointer — the one thing left of a widget's own look. The card's From list is
/// drawn with it, in either container (`compose_card`); the fields that are
/// flat wear
/// `Field::flat` instead, which takes their box off without touching the
/// colour of the text selected inside them.
pub(super) fn flatten_widgets(ui: &mut egui::Ui, hover: egui::Color32) {
    take_box_off(ui);
    let widgets = &mut ui.visuals_mut().widgets;
    widgets.hovered.weak_bg_fill = hover;
    widgets.active.weak_bg_fill = hover;
    widgets.open.weak_bg_fill = hover;
}
/// The frame a field wears when Kimo draws the box itself: the text's margin
/// and nothing else — no fill, no line. A frame of its own is also what keeps
/// egui from painting one, and from reaching for `Visuals::selection.stroke` to
/// paint it: that slot is the colour of *selected text* too, so a field that
/// took it for its outline would recolour every selection in the app. (The
/// accent is a fill and focus colour, not text — `theme`'s contrast test says
/// so.)
pub(super) fn field_frame(margin: egui::Margin) -> egui::Frame {
    egui::Frame::NONE.inner_margin(margin)
}
