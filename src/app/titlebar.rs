//! The floating title bar: its overlay, geometry, groups and cells,
//! window controls, search box, filter/sort cells and mail-action run.

use super::*;

/// The title bar's height. The integer form is the one the message list's
/// top padding wants (`egui::Margin` is in whole points), and the only place
/// the two could drift apart.
pub(super) const TITLEBAR_HEIGHT_I8: i8 = 40;
pub(super) const TITLEBAR_HEIGHT: f32 = TITLEBAR_HEIGHT_I8 as f32;
/// Every button in the title bar is a square of this size, centred in the bar's
/// height: the nav toggle, the compose and sync buttons, the mail actions, the
/// cog and the three window controls. One size for all of them is what makes
/// the bar read as one row of the same button, whichever group a cell is in.
pub(super) const TITLEBAR_BUTTON_SIZE: f32 = 32.0;
/// The clear space between two of those cells. Every group in the bar lays
/// itself out with it, so the same rhythm runs from the first cell to the
/// close button.
pub(super) const TITLEBAR_BUTTON_GAP: f32 = 6.0;
/// The cells' hover fill: a small rounded square, not the full-height block the
/// bar's buttons used to fill under the pointer. Small, because the cell is —
/// the search box's 8 pt on a 24 pt tall box would read as a pill on a 32 pt
/// square.
pub(super) const TITLEBAR_BUTTON_CORNER_RADIUS: u8 = 6;
/// How long the bar takes to turn from docked chrome into glass and back.
/// Short enough to feel like a state change rather than an animation.
pub(super) const TITLEBAR_FLOAT_TIME: f32 = 0.16;
/// What the bar's fill keeps of its alpha while it floats: the rest is the
/// blurred mail showing through. At 0.25 the blurred mail *is* the bar's body
/// and the fill is a tint over it; the bar's own icons and query stay crisp
/// because they are painted over that fill at full strength.
pub(super) const GLASS_ALPHA: f32 = 0.25;
/// The same for the search box's own fill, which is lighter than the bar's to
/// begin with: it keeps a little less of it, so the box is glass too and the
/// mail still shows through it. The stroke is what keeps it reading as a
/// field once the fill has gone.
pub(super) const GLASS_FIELD_ALPHA: f32 = 0.45;
/// Width of the title bar's search box — the whole box, `SEARCH_BOX_MARGIN`
/// included, which is what `TextEdit::desired_width` sizes and what
/// `titlebar_search_width` plans in. Fixed rather than filling: the box is
/// anchored where the message list starts (`titlebar_anchors`), and a box that
/// widened with the window would read as part of the columns rather than as the
/// list's own field.
pub(super) const TITLEBAR_SEARCH_WIDTH: f32 = 175.0;
/// The narrowest the box is allowed to get before the bar gives it up
/// altogether (`titlebar_search_width`): below this it stops reading as a
/// field, and the search popup is still a Ctrl+F away. The margins come out of
/// this width like anything else, so the query is left with most of it.
pub(super) const TITLEBAR_SEARCH_FLOOR: f32 = 96.0;
/// Height of that box: the bar's height less the breathing room above and
/// below it, which is what makes the box read as a control rather than as a
/// line of text in the bar.
pub(super) const TITLEBAR_SEARCH_HEIGHT: f32 = TITLEBAR_HEIGHT - 16.0;
/// The clear chrome the bar's anchored groups keep around themselves: between
/// the first group and the search box, between the box and the mail actions, and
/// between either of them and the drag surface. Not elastic — the groups are
/// placed at their columns' own edges, so this is the same gap however wide the
/// window is, and what gives way when the room runs out is the search box's
/// width and then the box itself.
pub(super) const TITLEBAR_GROUP_GAP: f32 = 8.0;
/// How close the mail actions may come to the window controls. The run is
/// anchored to the reading pane; when the pane is wide enough to hold it beside
/// the controls this is the clearance it keeps, and when it is not the run steps
/// left over the pane's own edge instead — and no further than
/// `MAIL_ACTION_OVERHANG`.
pub(super) const MAIL_ACTION_EDGE_GAP: f32 = 6.0;
/// How far the mail actions may sit over the reading pane's own edge before the
/// bar gives them up: the pane's card is inset from that edge by
/// `CONTENT_GUTTER` plus `CONTENT_PADDING.x`, so a run that hangs over
/// it is still over the pane's card and covers only the list's own margin. Past
/// that the run is not this pane's any more, and the mail's own card head —
/// which carries the same cells — is where the actions are.
pub(super) const MAIL_ACTION_OVERHANG: f32 = CONTENT_GUTTER + CONTENT_PADDING.x;
/// How long the mail actions take to fade in and out with the mail they act on.
/// The bar's own state-change time (`TITLEBAR_FLOAT_TIME`), because it is the
/// same kind of change: what the bar shows following what the app has.
pub(super) const MAIL_ACTION_FADE_TIME: f32 = 0.16;
/// The radius of the dot the sync cell carries while an account's last sync
/// stands failed, and the size of the spinner it becomes while one is running:
/// the cell is where a sweep is asked for, so it is where one is shown.
pub(super) const SYNC_BADGE_RADIUS: f32 = 3.0;
pub(super) const SYNC_SPINNER_SIZE: f32 = 20.0;
/// How large the mail actions' glyphs are drawn. Their outlines are designed in
/// a 24×24 box, which `glyph_point` scales to this.
pub(super) const MAIL_ACTION_GLYPH: f32 = 18.0;
/// The mail card's own action cells and the glyphs in them (`mail_sheet`'s
/// head row): the title bar's run in a smaller square — `TITLEBAR_BUTTON_GAP`
/// between the cells, as in the bar, and `titlebar_hover`'s fill under the
/// pointer, since it is the same kind of cell on another surface. Smaller
/// because on a card the four share their row with the sender, where in the
/// bar they share theirs with the window's own controls.
pub(super) const CARD_ACTION_SIZE: f32 = 26.0;
pub(super) const CARD_ACTION_GLYPH: f32 = 15.0;
/// How far the title bar's first cell starts in from the window's left edge —
/// the space added before it, as the left group's own first item. Wider than
/// the margin at the other end, which the window controls keep.
pub(super) const TITLEBAR_LEFT_INSET: f32 = 16.0;
/// How far the window controls end short of the window's own right edge, so the
/// close button's rounded fill is not flush against the frame — flush, it
/// reaches the window's corner.
pub(super) const TITLEBAR_RIGHT_MARGIN: f32 = 10.0;
/// How thick the window's own resize border is, in points: the strip along each
/// edge and the corner squares where two strips meet (`resize_grips`). The
/// window is undecorated (`main.rs`), so these strips are the whole of what a
/// pointer can resize it with.
pub(super) const RESIZE_GRIP: f32 = 5.0;
/// Carve the title bar into those rects. `controls` is how many window
/// controls the bar draws — `titlebar`'s own list, so the two cannot drift.
pub(super) fn titlebar_rects(bar: egui::Rect, controls: usize) -> TitlebarRects {
    // Every cell in the bar is the same square, centred in its height, so the
    // rects here are a cell's own band rather than the bar's full height: what
    // is drawn to them — and what a group is clipped to — is the button, not
    // the column of chrome around it.
    let top = bar.center().y - TITLEBAR_BUTTON_SIZE / 2.0;
    let cell = |left: f32, width: f32| {
        egui::Rect::from_min_size(
            egui::pos2(left, top),
            egui::vec2(width, TITLEBAR_BUTTON_SIZE),
        )
    };
    let controls_left = bar.right() - TITLEBAR_RIGHT_MARGIN - titlebar_cells_width(controls);
    let cog_left = controls_left - TITLEBAR_BUTTON_GAP - TITLEBAR_BUTTON_SIZE;
    let sync_left = cog_left - TITLEBAR_BUTTON_GAP - TITLEBAR_BUTTON_SIZE;
    TitlebarRects {
        content: egui::Rect::from_min_max(
            bar.min,
            egui::pos2(sync_left.clamp(bar.left(), bar.right()), bar.bottom()),
        ),
        sync: cell(sync_left, TITLEBAR_BUTTON_SIZE),
        cog: cell(cog_left, TITLEBAR_BUTTON_SIZE),
        controls: cell(
            controls_left,
            bar.right() - TITLEBAR_RIGHT_MARGIN - controls_left,
        ),
    }
}
/// What the title bar's minimize button sends. With minimize-to-tray on and a
/// tray registered the window is hidden rather than minimised — the tray's
/// Show raises it again — and without a tray it keeps the normal minimize, so
/// the window cannot be hidden with no way back.
pub(super) fn minimize_command(hide: bool) -> egui::ViewportCommand {
    if hide {
        egui::ViewportCommand::Visible(false)
    } else {
        egui::ViewportCommand::Minimized(true)
    }
}
/// What the title bar's close button sends. With close-to-tray on and a tray
/// registered the window is hidden rather than closed — the tray's Show raises
/// it again, and only its Quit ends the process — and without a tray it keeps
/// the normal close, so a window cannot be hidden with no way back.
pub(super) fn close_command(hide: bool) -> egui::ViewportCommand {
    if hide {
        egui::ViewportCommand::Visible(false)
    } else {
        egui::ViewportCommand::Close
    }
}
/// The width the bar gives the search box in the room that is left for it: its
/// own width while there is room, as narrow as its floor while the room is
/// short, and nothing at all below that. The box is the one thing the bar gives
/// up — the first group never gives way, and the mail actions are the pane's own
/// row rather than the room's leftovers — and the search popup stays a Ctrl+F
/// away when it is gone.
///
/// Monotone in `room`, which is what keeps a window being resized from
/// flickering between two shapes. Nothing here is a rect: the caller draws the
/// box inside the room it was given and clips it to that room.
pub(super) fn titlebar_search_width(room: f32) -> Option<f32> {
    if room >= TITLEBAR_SEARCH_WIDTH {
        Some(TITLEBAR_SEARCH_WIDTH)
    } else if room >= TITLEBAR_SEARCH_FLOOR {
        Some(room)
    } else {
        None
    }
}
/// Place the two groups the bar anchors to the columns below it: the search box
/// over the message list, the mail actions over the reading pane. Both edges are
/// the columns' own (`KimoApp::list_column`), so both follow the splitters that
/// move those columns, and both are clamped clear of the first group — the nav
/// toggle and the pen, which never give way because the compose button needs no
/// mail open.
///
/// The run is placed only where it belongs: at the pane's edge, or over it by no
/// more than `MAIL_ACTION_OVERHANG` when the window controls leave no room
/// there, and never within `MAIL_ACTION_EDGE_GAP` of those controls. A pane
/// too narrow to hold the run beside them gives it up, and the mail's own card
/// head — the same cells, on the mail itself — is where the actions are then.
pub(super) fn titlebar_anchors(
    bar: egui::Rect,
    first_end: f32,
    list_left: f32,
    pane_left: f32,
    cluster_left: f32,
) -> TitlebarAnchors {
    let width = titlebar_cells_width(MailAction::ALL.len());
    // The pane's edge, or as far left of it as the window controls leave — and
    // in the popup layout, where the pane is the whole window, the first group's
    // own end.
    let preferred = pane_left.max(first_end + TITLEBAR_GROUP_GAP);
    let left = preferred.min(cluster_left - MAIL_ACTION_EDGE_GAP - width);
    let fits = left >= first_end + TITLEBAR_GROUP_GAP
        && left + width + MAIL_ACTION_EDGE_GAP <= cluster_left + 0.5
        // The pane's own run while the pane is a column of its own: one that
        // hangs further over the pane's edge than the card's margin is over the
        // list, and no longer this pane's. A popup pane is the whole window, so
        // there its edge is behind the first group and there is nothing to hang
        // over.
        && (pane_left < first_end + TITLEBAR_GROUP_GAP || left >= pane_left - MAIL_ACTION_OVERHANG);
    let actions = fits.then(|| {
        egui::Rect::from_min_size(
            egui::pos2(left, bar.center().y - TITLEBAR_BUTTON_SIZE / 2.0),
            egui::vec2(width, TITLEBAR_BUTTON_SIZE),
        )
    });
    // The box's own width is planned first, exactly as it was before there were
    // cells to sit beside it: the box is the list's, and a bar that cannot hold
    // everything holds the box. What the box leaves is the cells' room — they are
    // one group with it, a cell's gap apart — and each cell is drawn while the
    // room reaches its own end: the filter first, then the sort, so a bar that
    // can hold one of the two holds the filter. The box's width never depends on
    // them, which is what keeps a window being resized from flickering.
    let search = list_left.max(first_end + TITLEBAR_GROUP_GAP);
    let next = actions.map_or(cluster_left, |actions| actions.left());
    let search_width = titlebar_search_width(next - search - TITLEBAR_GROUP_GAP);
    let mut cell_left = search + search_width.unwrap_or_default() + TITLEBAR_BUTTON_GAP;
    let room_for_cell = |left: f32| left + TITLEBAR_BUTTON_SIZE + TITLEBAR_GROUP_GAP <= next + 0.5;
    let cell = |left: f32| {
        egui::Rect::from_min_size(
            egui::pos2(left, bar.center().y - TITLEBAR_BUTTON_SIZE / 2.0),
            egui::vec2(TITLEBAR_BUTTON_SIZE, TITLEBAR_BUTTON_SIZE),
        )
    };
    let mut next_cell = || {
        let rect = room_for_cell(cell_left).then(|| cell(cell_left));
        if rect.is_some() {
            cell_left += TITLEBAR_BUTTON_SIZE + TITLEBAR_BUTTON_GAP;
        }
        rect
    };
    let filter = search_width.and_then(|_| next_cell());
    let sort = filter.and_then(|_| next_cell());
    TitlebarAnchors {
        search,
        search_width,
        filter,
        sort,
        actions,
    }
}
/// The window's own resize border (`window_resize_grips`), as eight grips: the
/// four edge strips with the corners taken out of them, and the four corner
/// squares. The corners are grips of their own so that every point on the border
/// answers with exactly one direction — an edge strip run corner to corner would
/// answer for the corner of whichever edge was asked first.
///
/// Each grip is `RESIZE_GRIP` thick and `grip` is clamped to half of each side,
/// so the window's interior — the columns, the bar, the mail — is never a grip,
/// however small the window is.
pub(super) fn resize_grips(viewport: egui::Rect) -> [ResizeGrip; 8] {
    use egui::viewport::ResizeDirection as Dir;

    let grip = RESIZE_GRIP
        .min(viewport.width() / 2.0)
        .min(viewport.height() / 2.0);
    let (left, top, right, bottom) = (
        viewport.left(),
        viewport.top(),
        viewport.right(),
        viewport.bottom(),
    );
    let rect = |x0: f32, y0: f32, x1: f32, y1: f32| {
        egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1))
    };
    [
        ResizeGrip {
            rect: rect(left, top, left + grip, top + grip),
            direction: Dir::NorthWest,
            cursor: egui::CursorIcon::ResizeNorthWest,
        },
        ResizeGrip {
            rect: rect(left + grip, top, right - grip, top + grip),
            direction: Dir::North,
            cursor: egui::CursorIcon::ResizeNorth,
        },
        ResizeGrip {
            rect: rect(right - grip, top, right, top + grip),
            direction: Dir::NorthEast,
            cursor: egui::CursorIcon::ResizeNorthEast,
        },
        ResizeGrip {
            rect: rect(right - grip, top + grip, right, bottom - grip),
            direction: Dir::East,
            cursor: egui::CursorIcon::ResizeEast,
        },
        ResizeGrip {
            rect: rect(right - grip, bottom - grip, right, bottom),
            direction: Dir::SouthEast,
            cursor: egui::CursorIcon::ResizeSouthEast,
        },
        ResizeGrip {
            rect: rect(left + grip, bottom - grip, right - grip, bottom),
            direction: Dir::South,
            cursor: egui::CursorIcon::ResizeSouth,
        },
        ResizeGrip {
            rect: rect(left, bottom - grip, left + grip, bottom),
            direction: Dir::SouthWest,
            cursor: egui::CursorIcon::ResizeSouthWest,
        },
        ResizeGrip {
            rect: rect(left, top + grip, left + grip, bottom - grip),
            direction: Dir::West,
            cursor: egui::CursorIcon::ResizeWest,
        },
    ]
}
/// The width of a run of `count` cells in the title bar: `TITLEBAR_BUTTON_SIZE`
/// squares, `TITLEBAR_BUTTON_GAP` apart. Every run is measured with this — the
/// mail actions (which are placed at the reading pane's own edge), the window
/// controls and the cells the sync and the cog are carved as — so no run can
/// disagree with the cells it draws.
pub(super) fn titlebar_cells_width(count: usize) -> f32 {
    count as f32 * TITLEBAR_BUTTON_SIZE + count.saturating_sub(1) as f32 * TITLEBAR_BUTTON_GAP
}
/// Clip a Ui to the group rect it was given, so that neither what it draws nor
/// what it senses reaches past that rect. `Ui::interact` hit-tests a widget
/// through its clip rect, which is what makes this a rule about clicks as much
/// as about pixels: a group the bar cannot fit is cut off at its edge, and a
/// click past that edge belongs to the group next to it.
pub(super) fn clip_to(ui: &mut egui::Ui, rect: egui::Rect) {
    let clip = ui.clip_rect().intersect(rect);
    ui.set_clip_rect(clip);
}
/// One button in the title bar: a `TITLEBAR_BUTTON_SIZE` square, centred in the
/// bar's height by the row that lays it out, empty at rest and filled with a
/// rounded corner under the pointer (`titlebar_hover`). Every button in the bar
/// is this — the nav toggle, compose, sync, the mail actions, the cog and the
/// window controls — and `enabled` is the one thing a caller varies: the mail
/// actions' disabled cells neither sense clicks nor fill, while the rest of the
/// bar is always live.
pub(super) fn titlebar_button(ui: &mut egui::Ui, enabled: bool) -> egui::Response {
    titlebar_button_faded(ui, enabled, 1.0)
}
/// The same cell, drawn at `fade` of its strength — the bar's own cell, for the
/// one run that has a state to come and go with (`titlebar_mail_actions`).
pub(super) fn titlebar_button_faded(ui: &mut egui::Ui, enabled: bool, fade: f32) -> egui::Response {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (_, response) = ui.allocate_exact_size(egui::Vec2::splat(TITLEBAR_BUTTON_SIZE), sense);
    if enabled {
        titlebar_hover_faded(ui, response, fade)
    } else {
        response
    }
}
/// The color a mail card's action glyph is drawn in: the scheme's weak text at
/// rest, the normal text color under the pointer. On a card the four glyphs
/// sit on the mail's own head, where the text color would make them the
/// loudest thing on it; the pointer is what says which one is being read, and
/// the hover fill is under it by then. A list row's own delete button draws
/// the same way (`text_weak` until the pointer finds it).
pub(super) fn card_action_color(scheme: ColorScheme, hovered: bool) -> egui::Color32 {
    if hovered {
        scheme.text
    } else {
        scheme.text_weak
    }
}
/// One of a mail card's own action cells (`mail_sheet`'s head row): the
/// title bar's button in a smaller square — `CARD_ACTION_SIZE` against
/// `TITLEBAR_BUTTON_SIZE` — with the same rest look and the same hover fill,
/// the glyph being the caller's either way.
pub(super) fn card_action_button(ui: &mut egui::Ui, enabled: bool) -> egui::Response {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (_, response) = ui.allocate_exact_size(egui::Vec2::splat(CARD_ACTION_SIZE), sense);
    if enabled {
        titlebar_hover(ui, response)
    } else {
        response
    }
}
/// Fill a title-bar cell while the pointer is over it. This is the treatment
/// every button in the bar shares, and the reason they are hand-painted rather
/// than `egui::Button`s: a stock button draws a card-colored frame with a
/// border, which in a title bar reads as a widget among window chrome. The
/// glyph is the caller's, painted afterwards so the fill cannot cover it.
pub(super) fn titlebar_hover(ui: &egui::Ui, response: egui::Response) -> egui::Response {
    titlebar_hover_faded(ui, response, 1.0)
}
/// The same fill at `fade` of its strength, for a cell that is fading in or
/// out with the thing it acts on (`titlebar_mail_actions`): the fill and the
/// glyph have to come and go together, or the pointer's cell would flash.
pub(super) fn titlebar_hover_faded(
    ui: &egui::Ui,
    response: egui::Response,
    fade: f32,
) -> egui::Response {
    if response.hovered() {
        titlebar_fill(
            ui,
            response.rect,
            ui.visuals().widgets.hovered.bg_fill.gamma_multiply(fade),
        );
    }
    response
}
/// The rounded fill itself, for the cells the bar places at exact x positions
/// rather than in a row (`titlebar_rects`): the cog and the window controls.
/// The close button passes the scheme's danger colour; everything else takes
/// the hover fill `titlebar_hover` does.
pub(super) fn titlebar_fill(ui: &egui::Ui, rect: egui::Rect, fill: egui::Color32) {
    ui.painter()
        .rect_filled(rect, TITLEBAR_BUTTON_CORNER_RADIUS, fill);
}

impl KimoApp {
    /// The title bar itself, floating over the columns rather than docked
    /// above them: one `egui::Area` fixed to the window's top edge and drawn
    /// after the panels, which is what lets the columns scroll under it.
    ///
    /// While the columns are at their top the bar is the solid chrome band it
    /// has always been; once one of them is scrolled it turns to glass — its
    /// backdrop blurred (`crate::glass`) and its fill translucent — with a
    /// line under it to say where it ends. Both follow one animated value, so
    /// scrolling back to the top docks the bar again instead of snapping it.
    pub(super) fn titlebar_area(&mut self, ctx: &egui::Context) {
        let scheme = theme::of(ctx);
        let rect = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(ctx.viewport_rect().width(), TITLEBAR_HEIGHT),
        );
        // Any column scrolling is the trigger: the navigation's folders, the
        // message list's offset, or the reading pane's card, which scrolls the
        // whole mail. Any scroll at all is content under the bar, and the top
        // is the only place where nothing is.
        let scrolled = self.nav_scroll.max(self.list_scroll).max(self.mail_scroll) > 0.5;
        let floating = ctx.animate_bool_with_time(
            egui::Id::new("titlebar-floating"),
            scrolled,
            TITLEBAR_FLOAT_TIME,
        );

        egui::Area::new(egui::Id::new("titlebar"))
            .order(egui::Order::Middle)
            .fixed_pos(egui::Pos2::ZERO)
            .movable(false)
            .show(ctx, |ui| {
                let painter = ui.painter().clone();
                // The blurred backdrop goes in first: a callback runs in the
                // order the shape was added, so it has to be under the fill
                // and the glyphs that sit on the glass.
                if floating > 0.0 {
                    self.glass.backdrop(&painter, rect);
                }
                // Opaque chrome at rest, glass while floating — the scheme's
                // `panel` either way, only its alpha moves, so the bar never
                // changes color, just solidity.
                painter.rect_filled(
                    rect,
                    0.0,
                    scheme
                        .panel
                        .gamma_multiply(1.0 - floating * (1.0 - GLASS_ALPHA)),
                );
                if floating > 0.0 {
                    // The line under a floating bar: the one thing that says
                    // where the bar ends and the content continues.
                    painter.hline(
                        rect.x_range(),
                        rect.bottom() - 0.5,
                        egui::Stroke::new(1.0_f32, scheme.row_edge.gamma_multiply(floating)),
                    );
                }
                // The bar's own widgets: the `Area` hands them a Ui the size
                // of the bar, so everything in `titlebar` lays out from the
                // window's top edge exactly as it did in the panel.
                ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                    self.titlebar(ui, floating);
                });
            });
    }
    /// The window's own resize border (`resize_grips`): eight strips in the
    /// foreground layer, one per edge and corner, that ask the compositor to
    /// resize the window from the edge under the pointer. The window is
    /// undecorated (`main.rs`), so there is no system border to do it, and on
    /// Wayland a client-requested `xdg_toplevel.resize` — what winit's
    /// `drag_resize_window` sends — is the only way an edge drag can resize at
    /// all.
    ///
    /// The press is the trigger, not egui's drag threshold: `BeginResize` hands
    /// the compositor the button that is down *now* (that is its contract), so a
    /// gesture that had to travel a few points first would spend them on
    /// nothing. Each strip is an area of its own rather than one ring around the
    /// window, so it shadows the columns only under the pointer; a maximized
    /// window has no edge to pull and gets none.
    pub(super) fn window_resize_grips(&self, ctx: &egui::Context) {
        if ctx.input(|input| input.viewport().maximized.unwrap_or(false)) {
            return;
        }
        for (index, grip) in resize_grips(ctx.viewport_rect()).into_iter().enumerate() {
            let response = egui::Area::new(egui::Id::new(("window-resize", index)))
                .order(egui::Order::Foreground)
                .fixed_pos(grip.rect.min)
                .movable(false)
                .sense(egui::Sense::click_and_drag())
                .show(ctx, |ui| ui.set_min_size(grip.rect.size()))
                .response;
            if !response.contains_pointer() {
                continue;
            }
            ctx.set_cursor_icon(grip.cursor);
            if ctx.input(|input| input.pointer.primary_pressed()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(grip.direction));
            }
        }
    }
    /// Custom window title bar (the window is created undecorated in
    /// `main.rs`). Left to right: the nav collapse toggle, the compose button,
    /// the account status, the search box, then the group of mail actions —
    /// and, at the window's own right end, the sync
    /// cell, the preferences cog and minimize / maximize / close. The leftover
    /// space drags the window (double-click toggles maximize).
    ///
    /// The bar is laid out as groups, not as one long row of widgets: the
    /// first group at the left — the nav toggle and the pen, which never give
    /// way — the sync cell, the cog and the window controls at the
    /// right, and the search box and the mail actions anchored to the columns
    /// under them (`titlebar_rects`, `titlebar_anchors`).
    /// Every group is drawn inside a rect of its own, clipped to it, and two
    /// things follow from that: a bar too narrow for what it carries gives
    /// pieces up in a stated order instead of stacking them on top of one
    /// another, and a group that somehow does not fit is cut off at its edge
    /// rather than drawn over its neighbour — and, since egui hit-tests a
    /// widget through its clip rect, the same holds for clicks.
    ///
    /// `floating` is how far the bar has gone to glass (see `titlebar_area`),
    /// which the search box follows: it is the one control in the bar with a
    /// fill of its own, and it would sit on the glass as a solid sticker.
    pub(super) fn titlebar(&mut self, ui: &mut egui::Ui, floating: f32) {
        let bar = ui.available_rect_before_wrap();
        let visuals = ui.visuals().clone();
        let scheme = theme::of(ui.ctx());
        let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));

        #[derive(Clone, Copy)]
        enum Ctrl {
            Minimize,
            Maximize,
            Close,
        }
        // Drawn right to left so Close ends up at the very right edge. This
        // list is also what `titlebar_rects` carves the bar's right end with,
        // so a control added here cannot leave the arithmetic behind.
        let buttons = [
            (Ctrl::Close, false),
            (Ctrl::Maximize, maximized),
            (Ctrl::Minimize, false),
        ];
        let rects = titlebar_rects(bar, buttons.len());
        let content = rects.content;

        // --- left group: the actions that belong to the window itself ---
        // Nav toggle, compose. Laid out from the bar's left edge, and where it
        // came to is what every other group is placed after.
        let left_end = ui
            .scope_builder(
                egui::UiBuilder::new()
                    .max_rect(content)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                |row| {
                    clip_to(row, content);
                    // The bar's own rhythm: these cells stand
                    // `TITLEBAR_BUTTON_GAP` apart exactly as the mail actions and
                    // the window controls do.
                    row.spacing_mut().item_spacing.x = TITLEBAR_BUTTON_GAP;
                    // The bar's own left margin, before the first cell: the
                    // toggle is the leftmost thing in the bar.
                    row.add_space(TITLEBAR_LEFT_INSET);

                    // Nav collapse toggle (hamburger, painted as lines). An
                    // icon cell rather than an `egui::Button`, so it carries no
                    // outline: the bar's icons only fill under the pointer.
                    let toggle = titlebar_button(row, true);
                    paint_nav_icon(row.painter(), toggle.rect, visuals.text_color());
                    if toggle.clicked() {
                        self.nav_collapsed = !self.nav_collapsed;
                    }
                    toggle.on_hover_text(if self.nav_collapsed {
                        "Show folders"
                    } else {
                        "Hide folders"
                    });

                    // Compose: the one mail action that needs no mail open, so
                    // it sits beside the nav toggle rather than in the group on
                    // the right, which acts on the mail in the reading pane. It
                    // opens the compose window (empty; a window that is already
                    // open is left as it is, draft and all).
                    let compose = titlebar_button(row, true);
                    paint_compose_icon(row.painter(), compose.rect, visuals.text_color());
                    if compose.clicked() {
                        self.open_compose();
                    }
                    compose.on_hover_text("Compose a new mail");

                    // No account menu here: the navigation column lists
                    // every account and its folders, so switching accounts is
                    // picking a folder there. The title bar keeps the global
                    // actions only (add/remove accounts lives in Preferences).
                    // Sync is not one of these cells: it belongs with the cog
                    // and the window controls at the right end (see below),
                    // because it is about the whole app rather than about the
                    // mail the pane shows.

                    row.min_rect().right()
                },
            )
            .inner;

        // --- the list's search box and the pane's mail actions ---
        // The bar's two anchored groups (`titlebar_anchors`): the box sits at
        // the message list's own left edge and the run at the reading pane's, so
        // each is over the column it belongs to and follows that column's
        // splitter. The first group is the one thing between them that never
        // gives way — the compose button needs no mail open — and both anchors
        // are clamped clear of it.
        let pane_left = if self.pane_popup {
            0.0
        } else {
            self.list_column.right()
        };
        let anchors = titlebar_anchors(
            bar,
            left_end,
            self.list_column.left(),
            pane_left,
            rects.sync.left(),
        );
        // The room the box has: up to whatever stands next to it on the right —
        // the pane's run, or the window controls when the run is not drawn.
        // (What the box leaves there is the filter cell's room, and the plan has
        // already decided whether either of them is drawn.)
        let next = anchors
            .actions
            .map_or(rects.sync.left(), |actions| actions.left());
        let mut box_end: Option<f32> = None;
        if let Some(box_width) = anchors.search_width {
            let middle = egui::Rect::from_min_max(
                egui::pos2(anchors.search, bar.top()),
                egui::pos2(anchors.search + box_width, bar.bottom()),
            );
            // Search, as one box: the query with the magnifier inside
            // Search, as one box: the query with the magnifier inside
            // its left edge, and the whole box is one click target —
            // the glyph and the text open the same popup, which is
            // where a query is typed (the scope, the filters and the
            // count live there too). The field is non-interactive for
            // that reason: a caret here would only fight the popup's
            // own field for the keyboard, and would leave focus behind
            // when the popup closes. The glyph takes the accent while a
            // search is what the message list shows.
            //
            // The box's frame is stock `TextEdit` chrome, so its
            // corner radius is the widget visuals' — rounded here in a
            // scope, which leaves every other field in the app with
            // egui's tighter default. The field is deliberately
            // non-interactive and egui picks the visuals by `Sense`
            // rather than by hover, so `noninteractive` is the slot it
            // actually draws with; the rest are rounded as well, so that
            // making the field live again would not square the corners.
            //
            // Its fill goes to glass with the bar: the fill keeps only
            // part of the input color, so the blurred mail behind the
            // bar shows through the box the way it shows through the
            // bar itself. The stock frame is drawn from
            // `Visuals::text_edit_bg_color`, which is why this is
            // `background_color` and not another widget slot.
            let box_rect = ui
                .scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(middle)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |row| {
                        clip_to(row, middle);
                        let margin = SEARCH_BOX_MARGIN;
                        let field = row
                            .scope(|ui| {
                                let radius = egui::CornerRadius::same(SEARCH_BOX_CORNER_RADIUS);
                                let widgets = &mut ui.style_mut().visuals.widgets;
                                for widget in [
                                    &mut widgets.noninteractive,
                                    &mut widgets.inactive,
                                    &mut widgets.hovered,
                                    &mut widgets.active,
                                    &mut widgets.open,
                                ] {
                                    widget.corner_radius = radius;
                                }
                                let fill = scheme
                                    .input
                                    .gamma_multiply(1.0 - floating * (1.0 - GLASS_FIELD_ALPHA));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.search.text)
                                        .hint_text("Search mail…")
                                        .interactive(false)
                                        .background_color(fill)
                                        // `desired_width` is the whole
                                        // box — the margins are taken out
                                        // of it, not added to it — which is
                                        // the one thing the fit and the
                                        // widget have to agree on.
                                        .desired_width(box_width)
                                        // The bar is taller than a text row, so the
                                        // field is grown to a fixed height and the
                                        // query (or the hint) is centred in it.
                                        .min_size(egui::vec2(0.0, TITLEBAR_SEARCH_HEIGHT))
                                        .vertical_align(egui::Align::Center)
                                        .margin(margin),
                                )
                            })
                            .inner;
                        // The widget's rect *is* the box: the whole frame,
                        // margins included, which is the rect `desired_width`
                        // sized and what the magnifier and the click target
                        // are placed from. (`TextEdit` used to hand back the
                        // inner text rect, which is where the margins came
                        // off it; the 0.36 rewrite sizes the frame instead,
                        // and re-adding them put the magnifier outside the
                        // box and the click target over the compose cell.)
                        let box_rect = field.rect;
                        // Registered after the field, which is why the click is
                        // the box's: it opens the popup wherever in the box it
                        // lands, the magnifier included.
                        let search = row
                            .interact(
                                box_rect,
                                egui::Id::new("titlebar-search"),
                                egui::Sense::click(),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                        if search.clicked() {
                            self.open_search();
                        }
                        // Grey while nothing is being asked for — the magnifier
                        // then matches the field's own hint text instead of
                        // reading as a live control — and the accent while a query
                        // is what the message list shows.
                        let search_color = if self.search.is_active() {
                            scheme.accent
                        } else {
                            visuals.weak_text_color()
                        };
                        let icon_rect = egui::Rect::from_center_size(
                            egui::pos2(
                                box_rect.left() + SEARCH_ICON_INSET as f32 / 2.0,
                                box_rect.center().y,
                            ),
                            egui::vec2(SEARCH_ICON_SIZE, SEARCH_ICON_SIZE),
                        );
                        paint_search_icon(row.painter(), icon_rect, search_color);
                        search.on_hover_text("Search mail — filters and scope (Ctrl+F)");
                        box_rect
                    },
                )
                .inner;
            box_end = Some(box_rect.right());
        }

        // --- the filter and sort cells: the search box's own companions ---
        // The column's two view controls, in the box's group rather than with the
        // pane's actions: they are about what the list *is* — which of its mail
        // is shown, and in what order — which is what the box is about too. Each
        // says its state the way the magnifier does, taking the accent while it
        // is doing something other than its default, and each opens its own menu
        // of two or three groups of choices.
        if let Some(filter) = anchors.filter {
            ui.scope_builder(
                egui::UiBuilder::new()
                    .max_rect(filter)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                |row| {
                    clip_to(row, filter);
                    let response = titlebar_button(row, true);
                    // The accent while any view is ticked, the bar's text colour
                    // while none is — the way the magnifier says a query is on.
                    let color = if self.list_filters.is_all() {
                        visuals.text_color()
                    } else {
                        scheme.accent
                    };
                    paint_filter_icon(row.painter(), response.rect, color, MAIL_ACTION_GLYPH);
                    let response = response
                        .on_hover_text(format!("Filter — showing {}", self.list_filters.summary()));
                    self.filter_popup(&response);
                },
            );
        }
        if let Some(sort) = anchors.sort {
            ui.scope_builder(
                egui::UiBuilder::new()
                    .max_rect(sort)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                |row| {
                    clip_to(row, sort);
                    let response = titlebar_button(row, true);
                    // The accent while the list is ordered by anything but the
                    // default: the sort is always doing something, so what the
                    // colour says is that the user asked for it.
                    let color = if self.list_sort == ListSort::default() {
                        visuals.text_color()
                    } else {
                        scheme.accent
                    };
                    paint_sort_icon(row.painter(), response.rect, color, MAIL_ACTION_GLYPH);
                    let response =
                        response.on_hover_text(format!("Sort — {}", self.list_sort.summary()));
                    self.sort_popup(&response);
                },
            );
        }

        // --- the pane's mail actions ---
        // The pane's own row, drawn only while there is a mail to act on, or a
        // selection — which is what Delete takes, and the one action that stays
        // live in select mode. Seven dead glyphs over a pane with no mail in it
        // said nothing and cluttered the bar; the row comes and goes over
        // `MAIL_ACTION_FADE_TIME` rather than blinking, because the mail being
        // opened or closed is what the bar is following. The cells and glyphs are
        // the bar's own, so the pane's card head — the same five cells brought
        // down onto the mail — reads as this row's other half.
        if let Some(actions) = anchors.actions {
            let wanted = self.has_selected_message() || !self.selected_mails.is_empty();
            let shown = ui.ctx().animate_bool_with_time(
                egui::Id::new("titlebar-actions"),
                wanted,
                MAIL_ACTION_FADE_TIME,
            );
            if shown > 0.0 {
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(actions)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |row| {
                        clip_to(row, actions);
                        self.titlebar_mail_actions(row, &visuals, shown);
                    },
                );
            }
        }

        // --- drag surface: the chrome the bar has left ---
        // The window is undecorated, so this is what moves it (and what
        // double-click maximizes it). With the groups anchored at both ends —
        // the first group at the left, the list's box and the pane's run at
        // their columns' edges — the bar has two stretches of it: the chrome
        // over the navigation column, between the first group and the list's
        // box, and the room between the box and the pane's run (or the window
        // controls, when no run is drawn). A bar with no room to spare simply
        // has none — the window controls have the window's right end either way.
        let mut free: Vec<(f32, f32)> = Vec::new();
        match box_end {
            // The box is drawn: two stretches, one on each side of it.
            Some(end) => {
                free.push((left_end, anchors.search));
                // The box and the cells beside it are one group, so what is left
                // of the bar starts after whichever of them ends last.
                let group_end = anchors
                    .sort
                    .or(anchors.filter)
                    .map_or(end, |cell| cell.right());
                free.push((group_end, next));
            }
            // No box — not even a narrow one: one stretch from the first group
            // to whatever comes next.
            None => free.push((left_end, next)),
        }
        for (index, (from, to)) in free.iter().enumerate() {
            if to - from <= 2.0 * TITLEBAR_GROUP_GAP {
                continue;
            }
            let rect = egui::Rect::from_min_max(
                egui::pos2(from + TITLEBAR_GROUP_GAP, bar.top()),
                egui::pos2(to - TITLEBAR_GROUP_GAP, bar.bottom()),
            );
            let drag = ui.interact(
                rect,
                egui::Id::new(("titlebar-drag", index)),
                egui::Sense::click_and_drag(),
            );
            if drag.drag_started() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            if drag.double_clicked() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            }
        }

        // --- right group: sync, the cog and the window controls ---
        // Fixed, and drawn last: they are what the rest of the bar is laid out
        // around, and the one thing a bar of any width keeps. The group's rect
        // starts at the sync cell, so the clip is also what says which half of
        // the bar a shape belongs to (see the no-overlap test in `tests`).
        let right = egui::Rect::from_min_max(egui::pos2(rects.sync.left(), bar.top()), bar.max);
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(right)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |row| {
                clip_to(row, right);
                // Sync now: the whole app's mail, from every account, on demand
                // — what the app does at startup and what the automatic sync
                // does on its clock, asked for by hand. The cell is the bar's
                // own, placed at the exact rect the arithmetic carved for it
                // like the cog and the window controls.
                //
                // **It is also where the app's sync state is shown**: a spinner
                // while any account is syncing, a dot while one's last sync
                // stands failed, and the accounts' own words in the tooltip.
                // That is what the strip beside it used to say, one letter at a
                // time; the cell is the better place for it — it is the control
                // a sweep is asked for from, it is always in the same place, and
                // nothing else in the bar has to move aside for it.
                let mut tooltip = String::from(
                    "Sync now — every account is asked for its mail, and the folder you have \
                     open is fetched too if it was never fetched.",
                );
                let mut failed = false;
                for account in &self.accounts {
                    let Some(state) = self.states.get(&account.id) else {
                        continue;
                    };
                    if state.syncing {
                        tooltip.push_str(&format!("\n{}: syncing…", account.name));
                    }
                    if let Some(error) = &state.error {
                        failed = true;
                        tooltip.push_str(&format!("\n{}: {error}", account.name));
                    }
                }
                let syncing = self.sync_in_flight();
                let sync_resp = titlebar_hover(
                    row,
                    row.interact(
                        rects.sync,
                        egui::Id::new("titlebar-sync"),
                        egui::Sense::click(),
                    ),
                );
                if syncing {
                    row.put(
                        rects.sync,
                        egui::Spinner::new()
                            .color(scheme.accent)
                            .size(SYNC_SPINNER_SIZE),
                    );
                } else {
                    paint_sync_icon(row.painter(), rects.sync, visuals.text_color());
                }
                if failed {
                    row.painter().circle_filled(
                        egui::pos2(rects.sync.right() - 7.0, rects.sync.top() + 7.0),
                        SYNC_BADGE_RADIUS,
                        scheme.danger,
                    );
                }
                if sync_resp.clicked() {
                    self.sync_all();
                }
                sync_resp.on_hover_text(tooltip);

                let cog_resp = titlebar_hover(
                    row,
                    row.interact(
                        rects.cog,
                        egui::Id::new("titlebar-preferences"),
                        egui::Sense::click(),
                    ),
                );
                paint_cog_icon(row.painter(), rects.cog, visuals.text_color());
                if cog_resp.clicked() {
                    self.show_preferences = true;
                }
                cog_resp.on_hover_text("Preferences");

                for (idx, (ctrl, is_max)) in buttons.iter().enumerate() {
                    // The list reads from the window's right edge inwards, which
                    // is where the cells are placed from: `buttons[0]` is the
                    // close button, and the run ends
                    // `TITLEBAR_RIGHT_MARGIN` short of the window's corner.
                    let rect = egui::Rect::from_min_size(
                        egui::pos2(
                            rects.controls.right()
                                - TITLEBAR_BUTTON_SIZE
                                - idx as f32 * (TITLEBAR_BUTTON_SIZE + TITLEBAR_BUTTON_GAP),
                            rects.controls.top(),
                        ),
                        egui::Vec2::splat(TITLEBAR_BUTTON_SIZE),
                    );
                    let resp = row.interact(
                        rect,
                        egui::Id::new(("titlebar-button", idx)),
                        egui::Sense::click(),
                    );
                    let fg = if resp.hovered() {
                        let bg = match ctrl {
                            Ctrl::Close => scheme.danger_hover,
                            _ => visuals.widgets.hovered.bg_fill,
                        };
                        titlebar_fill(row, rect, bg);
                        match ctrl {
                            // The scheme's on-danger color reads best on the
                            // close red; elsewhere keep the theme text color so
                            // the glyph survives light visuals.
                            Ctrl::Close => scheme.on_danger,
                            _ => visuals.text_color(),
                        }
                    } else {
                        visuals.text_color()
                    };
                    match (ctrl, is_max) {
                        (Ctrl::Minimize, _) => paint_minimize_icon(row.painter(), rect, fg),
                        (Ctrl::Maximize, false) => paint_maximize_icon(row.painter(), rect, fg),
                        (Ctrl::Maximize, true) => {
                            paint_restore_icon(row.painter(), rect, fg, visuals.panel_fill)
                        }
                        (Ctrl::Close, _) => paint_close_icon(row.painter(), rect, fg),
                    }
                    if resp.clicked() {
                        match ctrl {
                            Ctrl::Minimize => {
                                let hide = self.minimize_to_tray && crate::tray::available();
                                row.ctx().send_viewport_cmd(minimize_command(hide));
                            }
                            Ctrl::Maximize => row
                                .ctx()
                                .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized)),
                            Ctrl::Close => {
                                let hide = self.close_to_tray && crate::tray::available();
                                row.ctx().send_viewport_cmd(close_command(hide));
                            }
                        }
                    }
                }
            },
        );
    }
    /// The mail actions the title bar carries — reply, reply all, forward,
    /// label, flag, export, delete — in one run to the right of the search box.
    /// They act on the mail open in the reading pane, so that is what enables
    /// them (`has_selected_message`): with no mail there the seven glyphs are
    /// dimmed, the buttons stop sensing clicks and nothing fills under the
    /// pointer. **Select mode is the other answer**: the list is what the click
    /// is about then, and Delete — the one action that has something to do with
    /// more than one mail — is the only one of the seven left enabled
    /// (`mail_action_enabled`).
    ///
    /// Every cell is one of the bar's buttons — `TITLEBAR_BUTTON_SIZE` square,
    /// `TITLEBAR_BUTTON_GAP` from the next one, empty at rest and filled under
    /// the pointer with a small rounded corner (`titlebar_button`) — which is
    /// what keeps the seven of them reading as one group rather than as seven
    /// framed widgets. They are the same cells the nav toggle, the compose and
    /// sync buttons, the cog and the window controls are; what makes this run a
    /// group of its own is the clear chrome in front of it
    /// (`TITLEBAR_GROUP_GAP`), not a size or a fill only it has.
    ///
    /// Export and Delete are wired: Export hands the mail to
    /// `export_selected_message`, which has a worker write the server's own
    /// copy of it out as a `.eml`, and Delete hands it to
    /// `delete_selected_message`, which takes it off the server before it
    /// leaves the database. The other four are still drawn only — a click on
    /// one runs nothing, so they stay UI until the actions they name exist.
    pub(super) fn titlebar_mail_actions(
        &mut self,
        row: &mut egui::Ui,
        visuals: &egui::Visuals,
        fade: f32,
    ) {
        // The run lays itself out: `TITLEBAR_BUTTON_GAP` between the cells, which
        // is the gap `titlebar_cells_width` assumes.
        row.spacing_mut().item_spacing.x = TITLEBAR_BUTTON_GAP;
        // The one action with a state of its own: a flagged mail's cell says so
        // in the scheme's warning colour — the same tint the mail's row and card
        // are filled with — rather than only taking a click.
        let flagged = self.selected_mail().is_some_and(|mail| mail.flagged);
        for action in MailAction::ALL {
            let enabled = self.mail_action_enabled(action);
            let response = titlebar_button_faded(row, enabled, fade);
            let color = if enabled && action == MailAction::Flag && flagged {
                crate::theme::of(row.ctx()).warn
            } else if enabled {
                visuals.text_color()
            } else {
                visuals.weak_text_color()
            }
            .gamma_multiply(fade);
            let painter = row.painter();
            match action {
                MailAction::Reply => {
                    paint_reply_icon(painter, response.rect, color, MAIL_ACTION_GLYPH)
                }
                MailAction::ReplyAll => {
                    paint_reply_all_icon(painter, response.rect, color, MAIL_ACTION_GLYPH)
                }
                MailAction::Forward => {
                    paint_forward_icon(painter, response.rect, color, MAIL_ACTION_GLYPH)
                }
                MailAction::Label => {
                    paint_label_icon(painter, response.rect, color, MAIL_ACTION_GLYPH)
                }
                MailAction::Flag => {
                    paint_flag_icon(painter, response.rect, color, MAIL_ACTION_GLYPH)
                }
                MailAction::Export => paint_export_icon(painter, response.rect, color),
                MailAction::Delete => {
                    paint_delete_icon(painter, response.rect, color, MAIL_ACTION_GLYPH)
                }
            }
            let response = response.on_hover_text(if enabled {
                if self.select_mode && action == MailAction::Delete {
                    delete_selection_tooltip(self.selected_mails.len())
                } else if action == MailAction::Flag {
                    // The click takes the flag off now, so the hover text says
                    // that rather than what a flag is.
                    MailAction::flag_tooltip(flagged).to_string()
                } else {
                    action.tooltip().to_string()
                }
            } else if self.select_mode {
                if action == MailAction::Delete {
                    "Delete — no mail selected".to_string()
                } else {
                    format!("{} — not available in select mode", action.label())
                }
            } else {
                format!("{} — no mail selected", action.label())
            });
            // The label cell carries the popup rather than running one action
            // (`label_popup`): picking several labels is what it is for, so the
            // menu stays open across the clicks in it, and it is drawn on every
            // frame the cell is. The box starts empty for each mail it is opened
            // on: what was typed for the last one is not a draft for this.
            if enabled && action == MailAction::Label {
                if let Some(key) = self.selected_mail_key() {
                    if response.clicked() {
                        self.label_draft.clear();
                    }
                    self.label_popup(&response, &key);
                }
            } else if enabled && response.clicked() {
                match action {
                    MailAction::Export => self.export_selected_message(),
                    MailAction::Delete => self.delete_selected_message(),
                    // The bar's three answers to a mail come up as popups
                    // over everything (`ComposeSpot::Popup`); the mail's own
                    // header opens the same three on the spot, in the pane
                    // the mail is in. The bar answers the mail the pane is
                    // showing — the header, the mail it sits on.
                    MailAction::Reply | MailAction::ReplyAll | MailAction::Forward => {
                        if let Some(mail) = self.selected_mail().cloned() {
                            self.compose_about(action, ComposeSpot::Popup, &mail);
                        }
                    }
                    MailAction::Label => {}
                    MailAction::Flag => self.toggle_flag(),
                }
            }
        }
    }
}
