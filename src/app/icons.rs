//! Vector glyphs and the small shared painters used by buttons, rows, folders,
//! panels and menus.

use super::*;

/// The send glyph: a paper plane — a dart pointing right, its two wings meeting
/// at the fold. Two convex fills rather than one path, because that is what the
/// tessellator wants and a notch is not convex.
pub(super) fn paint_send_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let r = rect.width().min(rect.height()) / 2.0;
    let at = |x: f32, y: f32| egui::pos2(c.x + r * x, c.y + r * y);
    // A dart pointing right: the tip, the two back corners, and the fold
    // between them — indented to the *right* of the back edge, which is what
    // leaves two wings of equal area. (A fold left of that line flattens the
    // lower wing into a sliver, and the whole glyph then reads as a slash.)
    let tip = at(0.95, 0.0);
    let back_top = at(-0.75, -0.85);
    let notch = at(-0.3, 0.0);
    let back_bottom = at(-0.75, 0.85);
    p.add(egui::Shape::convex_polygon(
        vec![tip, back_top, notch],
        color,
        egui::Stroke::NONE,
    ));
    p.add(egui::Shape::convex_polygon(
        vec![tip, notch, back_bottom],
        color,
        egui::Stroke::NONE,
    ));
}
/// The chevron a flat row of the answer ends with: the To row's, down while
/// the Cc and Bcc lines are folded away — the way to them — and up once they
/// are under the row, and a flat combo's own (`paint_combo_chevron`), which
/// reads the same way over the list it opens. The thread rows' chevron turned
/// a quarter (`paint_chevron`): the same two strokes meeting at a point, and
/// nothing else.
pub(super) fn paint_fold_chevron(
    painter: &egui::Painter,
    centre: egui::Pos2,
    size: f32,
    open: bool,
    color: egui::Color32,
) {
    let half = size / 2.0;
    let points = if open {
        vec![
            egui::pos2(centre.x - half, centre.y + half * 0.45),
            egui::pos2(centre.x, centre.y - half * 0.55),
            egui::pos2(centre.x + half, centre.y + half * 0.45),
        ]
    } else {
        vec![
            egui::pos2(centre.x - half, centre.y - half * 0.45),
            egui::pos2(centre.x, centre.y + half * 0.55),
            egui::pos2(centre.x + half, centre.y - half * 0.45),
        ]
    };
    painter.add(egui::Shape::line(points, egui::Stroke::new(1.0_f32, color)));
}
/// The chevron a combo the app draws flat ends with, in the place egui would put
/// the filled triangle a combo box carries: the To row's own drawing
/// (`paint_fold_chevron`), down while the list is away and up while it is open
/// — the same glyph, at the same size, and the same colour under the pointer
/// (`card_action_color`) as the row it stands on, because these are a card's
/// lines rather than a form of boxes: the compose card's From list, in either
/// container, and the search popup's scope.
///
/// `place` is what the combo's own hook handed back — where its triangle would
/// have been, the one spot the button's own layout is known from — and `list`
/// is the button's response, which is what says the pointer is on it. The combo
/// is given a hook that paints nothing: this is what stands in its place.
pub(super) fn paint_combo_chevron(
    ui: &egui::Ui,
    list: &egui::Response,
    place: egui::Rect,
    open: bool,
) {
    let scheme = theme::of(ui.ctx());
    paint_fold_chevron(
        ui.painter(),
        place.center(),
        COMPOSE_FOLD_GLYPH,
        open,
        card_action_color(scheme, list.hovered()),
    );
}
/// The attachment glyph: a page with the top-right corner folded over. One
/// drawing for every kind of file — what a card says about the kind is the MIME
/// type on its second line, which is the sender's own word for it.
pub(super) fn paint_file_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.3_f32, color);
    let fold = rect.width() * 0.36;
    let (l, t, r, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    p.add(egui::Shape::line(
        vec![
            egui::pos2(l, t),
            egui::pos2(r - fold, t),
            egui::pos2(r, t + fold),
            egui::pos2(r, b),
            egui::pos2(l, b),
            egui::pos2(l, t),
        ],
        stroke,
    ));
    p.add(egui::Shape::line(
        vec![
            egui::pos2(r - fold, t),
            egui::pos2(r - fold, t + fold),
            egui::pos2(r, t + fold),
        ],
        stroke,
    ));
}
/// Draw one line of the message list, heavier when the mail is unread.
///
/// epaint has no synthetic bold (`FontTweak` only scales and shifts glyphs),
/// and the installed face may ship no bold at all — Adwaita Sans, the default
/// on this desktop, has only Regular and Italic. Striking the same galley twice,
/// half a point apart, thickens the stems enough to read as bold at list sizes,
/// and costs nothing: the layout was already measured for one strike, so the
/// row cannot reflow.
pub(super) fn paint_row_text(
    painter: &egui::Painter,
    pos: egui::Pos2,
    galley: std::sync::Arc<egui::Galley>,
    color: egui::Color32,
    bold: bool,
) {
    painter.galley(pos, galley.clone(), color);
    if bold {
        painter.galley(pos + egui::vec2(BOLD_STRIKE, 0.0), galley, color);
    }
}
/// Decode the app's embedded icon for Preferences → About. It is the same
/// 256 px PNG the tray serves and notifications write (`crate::tray::APP_PNG`),
/// uploaded lazily so a session that never opens the page pays nothing. A
/// failed decode leaves the page worded only, which is a cosmetic loss.
pub(super) fn load_app_icon(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let decoded = image::load_from_memory(crate::tray::APP_PNG)
        .ok()?
        .to_rgba8();
    let size = [decoded.width() as usize, decoded.height() as usize];
    let pixels = egui::ColorImage::from_rgba_unmultiplied(size, decoded.as_raw());
    Some(ctx.load_texture("app-icon", pixels, egui::TextureOptions::LINEAR))
}
/// Decode the embedded folder icons (see `folders::FolderKind::ICONS`) into
/// textures, once at startup. A failed decode is skipped: that row then draws
/// without its icon, which is a cosmetic loss at worst.
pub(super) fn load_folder_icons(
    ctx: &egui::Context,
) -> HashMap<(FolderKind, bool), egui::TextureHandle> {
    FolderKind::ICONS
        .iter()
        .filter_map(|(kind, open, png)| {
            let decoded = image::load_from_memory(png).ok()?.to_rgba8();
            let size = [decoded.width() as usize, decoded.height() as usize];
            let pixels = egui::ColorImage::from_rgba_unmultiplied(size, decoded.as_raw());
            let name = format!("folder-{kind:?}-{open}");
            Some((
                (*kind, *open),
                ctx.load_texture(name, pixels, Default::default()),
            ))
        })
        .collect()
}
pub(super) fn paint_minimize_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let y = rect.center().y + 4.0;
    let x = rect.center().x;
    p.line_segment(
        [egui::pos2(x - 5.0, y), egui::pos2(x + 5.0, y)],
        egui::Stroke::new(1.5_f32, color),
    );
}
pub(super) fn paint_maximize_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    p.rect_stroke(
        egui::Rect::from_center_size(rect.center(), egui::vec2(10.0, 10.0)),
        0.0,
        egui::Stroke::new(1.2_f32, color),
        egui::StrokeKind::Middle,
    );
}
pub(super) fn paint_restore_icon(
    p: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    bg: egui::Color32,
) {
    let c = rect.center();
    let back = egui::Rect::from_min_size(egui::pos2(c.x - 2.0, c.y - 6.0), egui::vec2(10.0, 10.0));
    let front = egui::Rect::from_min_size(egui::pos2(c.x - 6.0, c.y - 4.0), egui::vec2(10.0, 10.0));
    p.rect_stroke(
        back,
        0.0,
        egui::Stroke::new(1.2_f32, color),
        egui::StrokeKind::Middle,
    );
    // Occlude the back window where they overlap so the two read as stacked.
    p.rect_filled(front, 0.0, bg);
    p.rect_stroke(
        front,
        0.0,
        egui::Stroke::new(1.2_f32, color),
        egui::StrokeKind::Middle,
    );
}
pub(super) fn paint_close_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let r = 4.5;
    p.line_segment(
        [egui::pos2(c.x - r, c.y - r), egui::pos2(c.x + r, c.y + r)],
        egui::Stroke::new(1.5_f32, color),
    );
    p.line_segment(
        [egui::pos2(c.x - r, c.y + r), egui::pos2(c.x + r, c.y - r)],
        egui::Stroke::new(1.5_f32, color),
    );
}
/// Circular-arrow (refresh) glyph for the sync button: an open ring with an
/// arrow head where it ends, turning the way the ring is drawn.
pub(super) fn paint_sync_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let r = 5.0;
    let stroke = egui::Stroke::new(1.3_f32, color);

    // Clockwise on screen (y grows downwards) from lower-right to upper-right,
    // leaving a gap on the right for the arrow head.
    let start = 0.6_f32;
    let end = std::f32::consts::TAU - 0.6;
    const STEPS: usize = 20;
    let ring: Vec<egui::Pos2> = (0..=STEPS)
        .map(|i| {
            let angle = start + (end - start) * i as f32 / STEPS as f32;
            c + egui::vec2(angle.cos(), angle.sin()) * r
        })
        .collect();
    p.add(egui::Shape::line(ring, stroke));

    // Head at the end of the ring, pointing along the tangent.
    let dir = egui::vec2(end.cos(), end.sin());
    let tangent = egui::vec2(-end.sin(), end.cos());
    let base = c + dir * r;
    p.add(egui::Shape::convex_polygon(
        vec![
            base + tangent * 3.4,
            base + dir * 2.1 - tangent * 0.3,
            base - dir * 2.1 - tangent * 0.3,
        ],
        color,
        egui::Stroke::NONE,
    ));
}
/// Magnifier glyph for the title-bar search button and the results strip: a
/// ring with a handle towards the lower right.
pub(super) fn paint_search_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center() - egui::vec2(1.0, 1.0);
    let r = 4.5;
    let stroke = egui::Stroke::new(1.4_f32, color);
    p.circle_stroke(c, r, stroke);
    let start = c + egui::vec2(r, r) * std::f32::consts::FRAC_1_SQRT_2;
    p.line_segment([start, start + egui::vec2(3.4, 3.4)], stroke);
}
/// Nav collapse glyph for the title-bar toggle: three stacked lines, drawn at
/// the size of the window controls' glyphs rather than of the cell they sit in
/// (`TITLEBAR_BUTTON_SIZE`), which every button in the bar shares.
pub(super) fn paint_nav_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let stroke = egui::Stroke::new(1.5_f32, color);
    for i in -1i32..=1 {
        let y = c.y + i as f32 * 4.0;
        p.line_segment([egui::pos2(c.x - 7.0, y), egui::pos2(c.x + 7.0, y)], stroke);
    }
}
/// Cog / gear glyph for the preferences button: a hub ring with eight teeth.
pub(super) fn paint_cog_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let r = 5.5;
    let stroke = egui::Stroke::new(1.2_f32, color);
    p.circle_stroke(c, r * 0.55, stroke);
    for i in 0..8 {
        let angle = i as f32 * std::f32::consts::TAU / 8.0;
        let (sin, cos) = angle.sin_cos();
        let dir = egui::vec2(cos, sin);
        p.line_segment([c + dir * (r * 0.62), c + dir * r], stroke);
    }
}
/// Map a point of a 24×24 outline onto a box of `size` centred on `center`.
pub(super) fn outline_point(center: egui::Pos2, x: f32, y: f32, size: f32) -> egui::Pos2 {
    center + egui::vec2(x - 12.0, y - 12.0) * (size / 24.0)
}
/// Map a point of one of those 24×24 outlines onto a title-bar cell, centred
/// on `center` and drawn at `MAIL_ACTION_GLYPH`.
pub(super) fn glyph_point(center: egui::Pos2, x: f32, y: f32) -> egui::Pos2 {
    outline_point(center, x, y, MAIL_ACTION_GLYPH)
}
/// The widget id of one of a row's controls — its checkbox, or the delete button
/// it offers under the pointer — or of the list's select-all row: one per row,
/// stable between frames the way the chevron's key is. `tag` keeps a
/// conversation's controls apart from the ones of the mail it holds — the two can
/// be the same mail — and the account and folder keep a search's hits apart,
/// since two accounts can hold one uid.
pub(super) fn row_widget_id(tag: &'static str, account: &str, folder: &str, key: &str) -> egui::Id {
    egui::Id::new((tag, account, folder, key))
}
/// The checkbox a row draws in place of its avatar: `Off` is an outline — the
/// accent under the pointer, which is what says it can be clicked — and `On` is
/// a filled box with the scheme's check on it. `Partial`, which only a
/// conversation with some of its mails checked reaches, is the same fill with a
/// dash: the box stands for the whole thread, and the thread is partly in.
pub(super) fn paint_checkbox(
    painter: &egui::Painter,
    rect: egui::Rect,
    state: CheckState,
    hovered: bool,
    scheme: ColorScheme,
) {
    match state {
        CheckState::Off => {
            let color = if hovered {
                scheme.accent
            } else {
                scheme.text_weak
            };
            painter.rect_stroke(
                rect,
                CHECKBOX_CORNER_RADIUS,
                egui::Stroke::new(1.0_f32, color),
                egui::StrokeKind::Inside,
            );
        }
        CheckState::On | CheckState::Partial => {
            painter.rect_filled(rect, CHECKBOX_CORNER_RADIUS, scheme.accent);
            let stroke = egui::Stroke::new(CHECKBOX_STROKE, scheme.on_accent);
            if state == CheckState::On {
                // The check: a short stroke down into the corner and a long one
                // back up out of it — one path, two turns, and no glyph from
                // the icon set, since a checkbox is not one of the bar's own
                // 24×24 outlines.
                let g = |x: f32, y: f32| {
                    egui::pos2(
                        rect.left() + rect.width() * x,
                        rect.top() + rect.height() * y,
                    )
                };
                painter.add(egui::Shape::line(
                    vec![g(0.26, 0.52), g(0.43, 0.7), g(0.74, 0.32)],
                    stroke,
                ));
            } else {
                let mid = rect.center().y;
                painter.line_segment(
                    [
                        egui::pos2(rect.left() + 4.0, mid),
                        egui::pos2(rect.right() - 4.0, mid),
                    ],
                    stroke,
                );
            }
        }
    }
}
/// The chevron a conversation's row carries at its left: a line drawing — two
/// strokes meeting at a point — rather than a filled triangle, pointing right
/// while the thread's mails are hidden and down while they are under it.
pub(super) fn paint_chevron(
    painter: &egui::Painter,
    centre: egui::Pos2,
    size: f32,
    open: bool,
    color: egui::Color32,
) {
    let half = size / 2.0;
    let points = if open {
        vec![
            egui::pos2(centre.x - half, centre.y - half * 0.45),
            egui::pos2(centre.x, centre.y + half * 0.55),
            egui::pos2(centre.x + half, centre.y - half * 0.45),
        ]
    } else {
        vec![
            egui::pos2(centre.x - half * 0.45, centre.y - half),
            egui::pos2(centre.x + half * 0.55, centre.y),
            egui::pos2(centre.x - half * 0.45, centre.y + half),
        ]
    };
    painter.add(egui::Shape::line(points, egui::Stroke::new(1.0_f32, color)));
}
/// The left-pointing chevron of the popup's back button: the thread rows'
/// chevron (`paint_chevron`) mirrored, drawn with the stroke the title bar's
/// glyph outlines use. Chevrons are the app's `back / open` glyph — the
/// thread rows' opens a conversation, this one leaves the popup pane — so
/// the one shape is what both of them draw.
pub(super) fn paint_back_chevron(
    painter: &egui::Painter,
    centre: egui::Pos2,
    color: egui::Color32,
) {
    let half = POPUP_BACK_CHEVRON / 2.0;
    let points = vec![
        egui::pos2(centre.x + half * 0.45, centre.y - half),
        egui::pos2(centre.x - half * 0.55, centre.y),
        egui::pos2(centre.x + half * 0.45, centre.y + half),
    ];
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(POPUP_BACK_STROKE, color),
    ));
}
/// The paperclip a message row draws when the mail has parts: the wire of a
/// clip, drawn as the one path it is — down, a half turn, up, over the top,
/// down, and a wide half turn back up — the way the title bar's outlines are
/// drawn, at `rect`'s size and in the caller's color.
pub(super) fn paint_clip_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let scale = rect.width() / 24.0;
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, rect.width());
    let stroke = egui::Stroke::new(2.0_f32 * scale, color);
    let up = egui::vec2(0.0, -1.0);
    let down = egui::vec2(0.0, 1.0);
    let left = egui::vec2(-1.0, 0.0);
    let right = egui::vec2(1.0, 0.0);

    // The inner loop, from its free end at the top: down, round the bottom and
    // up the other side.
    p.add(egui::Shape::line(vec![g(9.6, 8.5), g(9.6, 15.5)], stroke));
    wire_turn(p, g(9.6, 15.5), down, right, 2.4 * scale, stroke);
    p.add(egui::Shape::line(vec![g(14.4, 15.5), g(14.4, 6.0)], stroke));
    // Over the top and down the outside.
    wire_turn(p, g(14.4, 6.0), up, right, 2.05 * scale, stroke);
    p.add(egui::Shape::line(vec![g(18.5, 6.0), g(18.5, 13.5)], stroke));
    // The wide turn under the whole clip, and back up to the last free end.
    wire_turn(p, g(18.5, 13.5), down, left, 6.5 * scale, stroke);
    p.add(egui::Shape::line(vec![g(5.5, 13.5), g(5.5, 7.0)], stroke));
}
/// A half turn of a wire, drawn the way the icon set draws a corner: two quarter
/// turns, each a quadratic whose control is the corner its own tangents meet at
/// (`glyph_tail`), which is what makes one quadratic read as an arc at glyph
/// sizes. The wire arrives at `from` travelling along `dir` — a unit vector —
/// and leaves `2 * radius` away along `normal`, travelling the opposite way,
/// `normal` being the side the turn bulges toward.
pub(super) fn wire_turn(
    p: &egui::Painter,
    from: egui::Pos2,
    dir: egui::Vec2,
    normal: egui::Vec2,
    radius: f32,
    stroke: egui::Stroke,
) {
    let middle = from + radius * (dir + normal);
    let end = from + 2.0 * radius * normal;
    glyph_tail(p, from, from + radius * dir, middle, stroke);
    glyph_tail(p, middle, end + radius * dir, end, stroke);
}
/// The tail of the reply and forward arrows: a quarter turn rather than a
/// corner, drawn as the outline does it — a quadratic curve whose control sits
/// on the corner the two straight parts would have met at.
pub(super) fn glyph_tail(
    p: &egui::Painter,
    from: egui::Pos2,
    corner: egui::Pos2,
    to: egui::Pos2,
    stroke: egui::Stroke,
) {
    p.add(egui::epaint::QuadraticBezierShape::from_points_stroke(
        [from, corner, to],
        false,
        egui::Color32::TRANSPARENT,
        stroke,
    ));
}
/// Compose: a page with a pen writing on it — the page an outline of rounded
/// corners whose top-right one is left *open*, and the pen the straight stroke
/// that crosses exactly there. The two are one drawing: the pen passes through
/// the gap, so no stroke of the page ever touches it, which is what says the
/// pen is writing on the page rather than lying on top of it. Like the rest of
/// the bar's glyphs it comes from a 24×24 outline, so it needs no asset.
pub(super) fn paint_compose_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let g = |x: f32, y: f32| glyph_point(rect.center(), x, y);
    let stroke = egui::Stroke::new(1.5_f32, color);

    // The page, cut open where the pen crosses its top-right corner: the top
    // side, then the rest of the way round from where the right side starts
    // again. A corner is a quarter turn drawn the way the icon set draws them
    // — a quadratic whose control is the corner itself (`glyph_tail`).
    p.add(egui::Shape::line(vec![g(7.6, 5.0), g(16.0, 5.0)], stroke));
    p.add(egui::Shape::line(vec![g(20.0, 8.8), g(20.0, 17.4)], stroke));
    glyph_tail(p, g(20.0, 17.4), g(20.0, 21.0), g(16.4, 21.0), stroke);
    p.add(egui::Shape::line(vec![g(16.4, 21.0), g(7.6, 21.0)], stroke));
    glyph_tail(p, g(7.6, 21.0), g(4.0, 21.0), g(4.0, 17.4), stroke);
    p.add(egui::Shape::line(vec![g(4.0, 17.4), g(4.0, 8.6)], stroke));
    glyph_tail(p, g(4.0, 8.6), g(4.0, 5.0), g(7.6, 5.0), stroke);

    // The pen: its nib inside the page, its tip out past the corner.
    p.add(egui::Shape::line(vec![g(9.2, 17.2), g(21.6, 4.8)], stroke));
}
/// Paper: the contrast mark — a ring with one half of it filled, which is what
/// the cell switches between. Drawn at `size` like the card's other glyphs.
pub(super) fn paint_paper_icon(
    p: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    size: f32,
) {
    let centre = outline_point(rect.center(), 12.0, 12.0, size);
    let radius = size * 8.0 / 24.0;
    p.circle_stroke(centre, radius, egui::Stroke::new(1.5_f32, color));

    // The right half, filled: the arc from the ring's top round to its bottom,
    // closed through the centre, so what is painted is a half disc rather than a
    // polygon the ring would have to be cut out of.
    const STEPS: usize = 12;
    let mut half = Vec::with_capacity(STEPS + 2);
    half.push(centre);
    for step in 0..=STEPS {
        let angle =
            -std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * step as f32 / STEPS as f32;
        half.push(centre + egui::vec2(angle.cos(), angle.sin()) * radius);
    }
    p.add(egui::Shape::convex_polygon(half, color, egui::Stroke::NONE));
}
/// Reply: an arrow curving back to the left, its shaft turning down at the
/// tail. Every part is an open path, so the glyph reads as a line drawing like
/// the rest of the bar's icons. `size` is the glyph's own, as in
/// `paint_delete_icon`: the bar's cells draw it at `MAIL_ACTION_GLYPH`, the
/// mail card's smaller ones at `CARD_ACTION_GLYPH`.
pub(super) fn paint_reply_icon(
    p: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    size: f32,
) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
    let stroke = egui::Stroke::new(1.5_f32, color);
    p.add(egui::Shape::line(
        vec![g(20.0, 18.0), g(20.0, 16.0)],
        stroke,
    ));
    glyph_tail(p, g(20.0, 16.0), g(20.0, 12.0), g(16.0, 12.0), stroke);
    p.add(egui::Shape::line(vec![g(16.0, 12.0), g(4.0, 12.0)], stroke));
    p.add(egui::Shape::line(
        vec![g(9.0, 17.0), g(4.0, 12.0), g(9.0, 7.0)],
        stroke,
    ));
}
/// Reply all: reply's arrow with a second head behind the first. Drawn at
/// `size`, like `paint_reply_icon`.
pub(super) fn paint_reply_all_icon(
    p: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    size: f32,
) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
    let stroke = egui::Stroke::new(1.5_f32, color);
    p.add(egui::Shape::line(
        vec![g(22.0, 18.0), g(22.0, 16.0)],
        stroke,
    ));
    glyph_tail(p, g(22.0, 16.0), g(22.0, 12.0), g(18.0, 12.0), stroke);
    p.add(egui::Shape::line(vec![g(18.0, 12.0), g(7.0, 12.0)], stroke));
    p.add(egui::Shape::line(
        vec![g(12.0, 17.0), g(7.0, 12.0), g(12.0, 7.0)],
        stroke,
    ));
    p.add(egui::Shape::line(
        vec![g(7.0, 17.0), g(2.0, 12.0), g(7.0, 7.0)],
        stroke,
    ));
}
/// Forward: reply's arrow mirrored — it leaves to the right instead. Drawn at
/// `size`, like `paint_reply_icon`.
pub(super) fn paint_forward_icon(
    p: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    size: f32,
) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
    let stroke = egui::Stroke::new(1.5_f32, color);
    p.add(egui::Shape::line(vec![g(4.0, 18.0), g(4.0, 16.0)], stroke));
    glyph_tail(p, g(4.0, 16.0), g(4.0, 12.0), g(8.0, 12.0), stroke);
    p.add(egui::Shape::line(vec![g(8.0, 12.0), g(20.0, 12.0)], stroke));
    p.add(egui::Shape::line(
        vec![g(15.0, 17.0), g(20.0, 12.0), g(15.0, 7.0)],
        stroke,
    ));
}
/// Label: a tag — a square turned so that it points right, with the hole near
/// its flat end.
pub(super) fn paint_label_icon(
    p: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    size: f32,
) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
    let stroke = egui::Stroke::new(1.5_f32, color);
    p.add(egui::Shape::line(
        vec![
            g(3.0, 3.0),
            g(12.0, 3.0),
            g(21.0, 12.0),
            g(12.0, 21.0),
            g(3.0, 12.0),
            g(3.0, 3.0),
        ],
        stroke,
    ));
    // The hole is a dot rather than a ring: at this size a ring closes up.
    p.circle_filled(g(7.5, 7.5), size * 0.055, color);
}
/// Flag: a pole with a swallow-tailed pennant on it.
/// Flag: a pole with a banner. It is the one glyph of the title bar's set that
/// is drawn outside it as well — a flagged mail's row in the list draws it at
/// the end of its title line — so it takes a `size` like reply's arrow rather
/// than being fixed at `MAIL_ACTION_GLYPH`, and its stroke is scaled with it the
/// way the paperclip's is (the clip is only ever drawn at the row's size).
pub(super) fn paint_flag_icon(
    p: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    size: f32,
) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
    let stroke = egui::Stroke::new(1.5_f32 * size / MAIL_ACTION_GLYPH, color);
    p.add(egui::Shape::line(vec![g(5.0, 21.0), g(5.0, 3.0)], stroke));
    p.add(egui::Shape::line(
        vec![
            g(5.0, 4.0),
            g(19.0, 4.0),
            g(15.0, 9.0),
            g(19.0, 14.0),
            g(5.0, 14.0),
        ],
        stroke,
    ));
}
/// Sort: two arrows side by side, one up and one down — the mark every list
/// with an order draws on its sort control. The bar's own, like the funnel: a
/// menu row that names a direction uses one arrow of the pair
/// (`MenuIcon::Newest`), so the cell and the rows are drawn from one idea.
pub(super) fn paint_sort_icon(
    p: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    size: f32,
) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
    let stroke = egui::Stroke::new(1.5_f32, color);
    // Up: the shaft, then the head above it.
    p.add(egui::Shape::line(vec![g(9.0, 20.0), g(9.0, 6.0)], stroke));
    p.add(egui::Shape::line(
        vec![g(4.5, 10.5), g(9.0, 6.0), g(13.5, 10.5)],
        stroke,
    ));
    // Down: the same, the other way round.
    p.add(egui::Shape::line(vec![g(15.0, 4.0), g(15.0, 18.0)], stroke));
    p.add(egui::Shape::line(
        vec![g(10.5, 13.5), g(15.0, 18.0), g(19.5, 13.5)],
        stroke,
    ));
}
/// Filter: a funnel, the one glyph of the bar's set that is not about a mail —
/// it says which mail the *list* is showing. Drawn like the rest: one stroked
/// path inside the 24×24 outline the set shares.
pub(super) fn paint_filter_icon(
    p: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    size: f32,
) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
    let stroke = egui::Stroke::new(1.5_f32, color);
    p.add(egui::Shape::line(
        vec![
            g(4.0, 6.0),
            g(20.0, 6.0),
            g(14.0, 13.0),
            g(14.0, 20.0),
            g(10.0, 20.0),
            g(10.0, 13.0),
            g(4.0, 6.0),
        ],
        stroke,
    ));
}
/// One menu row's glyph, in the 24×24 outline style the whole icon set is drawn
/// in, at `FILTER_ICON_SIZE`.
pub(super) fn paint_menu_icon(
    p: &egui::Painter,
    icon: MenuIcon,
    rect: egui::Rect,
    color: egui::Color32,
) {
    let size = FILTER_ICON_SIZE;
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
    let stroke = egui::Stroke::new(1.5_f32 * size / MAIL_ACTION_GLYPH, color);
    match icon {
        // The whole mailbox: an envelope, the one mark that means *mail* rather
        // than something about it.
        MenuIcon::All | MenuIcon::Unread => {
            p.add(egui::Shape::line(
                vec![
                    g(3.0, 5.0),
                    g(21.0, 5.0),
                    g(21.0, 16.0),
                    g(3.0, 16.0),
                    g(3.0, 5.0),
                ],
                stroke,
            ));
            // The flap, from the top corners down to the middle.
            p.add(egui::Shape::line(
                vec![g(3.0, 6.5), g(12.0, 12.0), g(21.0, 6.5)],
                stroke,
            ));
            if icon == MenuIcon::Unread {
                // A mail that has not been read: the envelope with the new-mail
                // pip at its lower corner, the one part of this set that is
                // filled.
                p.circle_filled(g(19.5, 19.5), 3.0 * size / 24.0, color);
            }
        }
        // The list's own mark for a flagged mail, at the menu's size.
        MenuIcon::Flagged => paint_flag_icon(p, rect, color, size),
        // Addressed to me: a person — a head and the shoulders under it.
        MenuIcon::ToMe | MenuIcon::ByFrom => {
            p.circle_stroke(g(12.0, 8.0), 3.6 * size / 24.0, stroke);
            p.add(egui::Shape::line(
                vec![
                    g(5.0, 19.5),
                    g(5.0, 17.0),
                    g(8.0, 14.0),
                    g(16.0, 14.0),
                    g(19.0, 17.0),
                    g(19.0, 19.5),
                ],
                stroke,
            ));
        }
        // What the mail carries, in the list's own paperclip.
        MenuIcon::HasFiles => paint_clip_icon(p, rect, color),
        // The sort's two directions: one arrow each, the same shaft and head
        // turned round.
        MenuIcon::Newest | MenuIcon::Oldest => {
            let down = icon == MenuIcon::Newest;
            let (tail, head_y) = if down { (5.0, 17.0) } else { (19.0, 7.0) };
            p.add(egui::Shape::line(
                vec![g(12.0, tail), g(12.0, head_y)],
                stroke,
            ));
            let (wing, tip) = if down {
                (head_y - 4.5, head_y)
            } else {
                (head_y + 4.5, head_y)
            };
            p.add(egui::Shape::line(
                vec![g(7.5, wing), g(12.0, tip), g(16.5, wing)],
                stroke,
            ));
        }
        // Sorted by date: a calendar — a box with its binding along the top and
        // the two days' ticks under it.
        MenuIcon::ByDate => {
            p.add(egui::Shape::line(
                vec![
                    g(4.0, 6.5),
                    g(20.0, 6.5),
                    g(20.0, 19.5),
                    g(4.0, 19.5),
                    g(4.0, 6.5),
                ],
                stroke,
            ));
            p.add(egui::Shape::line(vec![g(4.0, 10.5), g(20.0, 10.5)], stroke));
            p.add(egui::Shape::line(vec![g(8.5, 4.5), g(8.5, 8.0)], stroke));
            p.add(egui::Shape::line(vec![g(15.5, 4.5), g(15.5, 8.0)], stroke));
        }
        // Sorted by subject: the title's own two lines, the second shorter.
        MenuIcon::BySubject => {
            p.add(egui::Shape::line(vec![g(4.0, 9.0), g(20.0, 9.0)], stroke));
            p.add(egui::Shape::line(vec![g(4.0, 15.0), g(14.0, 15.0)], stroke));
        }
    }
}
/// The tick a menu row carries at its end when its view is on: one stroke, the
/// way the rest of the set is drawn.
pub(super) fn paint_check_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, FILTER_TICK_SIZE);
    let stroke = egui::Stroke::new(1.5_f32 * FILTER_TICK_SIZE / MAIL_ACTION_GLYPH, color);
    p.add(egui::Shape::line(
        vec![g(5.0, 12.5), g(10.0, 17.5), g(19.0, 6.5)],
        stroke,
    ));
}
/// Export: an arrow coming down into an open tray, the file leaving the app.
/// It is the one glyph of the set that points into something rather than along
/// the mail (reply, forward) or at the mail (label, flag, delete), which is
/// what keeps it from reading as part of the trash can next to it.
pub(super) fn paint_export_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let g = |x: f32, y: f32| glyph_point(rect.center(), x, y);
    let stroke = egui::Stroke::new(1.5_f32, color);
    p.add(egui::Shape::line(
        vec![g(4.0, 14.0), g(4.0, 21.0), g(20.0, 21.0), g(20.0, 14.0)],
        stroke,
    ));
    p.add(egui::Shape::line(vec![g(12.0, 3.0), g(12.0, 15.0)], stroke));
    p.add(egui::Shape::line(
        vec![g(7.0, 10.0), g(12.0, 15.0), g(17.0, 10.0)],
        stroke,
    ));
}
/// Delete: the trash can — lid, handle above it, and the open-topped body. The
/// two lines the outline draws inside the body are left out: at this size they
/// close up into a smudge and the can reads better empty. `size` is the glyph's
/// own size: the title bar's cells draw it at `MAIL_ACTION_GLYPH`, a mail row's
/// smaller button at `ROW_ACTION_GLYPH`.
pub(super) fn paint_delete_icon(
    p: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    size: f32,
) {
    let g = |x: f32, y: f32| outline_point(rect.center(), x, y, size);
    let stroke = egui::Stroke::new(1.5_f32, color);
    p.add(egui::Shape::line(vec![g(3.0, 6.0), g(21.0, 6.0)], stroke));
    p.add(egui::Shape::line(
        vec![g(8.0, 6.0), g(8.0, 3.0), g(16.0, 3.0), g(16.0, 6.0)],
        stroke,
    ));
    p.add(egui::Shape::line(
        vec![g(5.0, 6.0), g(5.0, 21.0), g(19.0, 21.0), g(19.0, 6.0)],
        stroke,
    ));
}
