use super::super::*;
use super::common::*;

/// The bar's own half of that: a disabled cell neither senses clicks nor
/// fills under the pointer. This puts the pointer on Reply and on Delete in
/// select mode and reads what each one drew.
#[test]
fn the_title_bar_dims_the_one_mail_actions_in_select_mode() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    app.selected_message = Some("0".to_string());
    let actions =
        mail_action_rect(&ctx, &mut app, 1400.0).expect("the bar gave the actions up at 1400 pt");
    let cell = |action: MailAction| {
        let index = MailAction::ALL
            .iter()
            .position(|candidate| *candidate == action)
            .expect("one of the seven");
        egui::pos2(
            actions.left()
                + index as f32 * (TITLEBAR_BUTTON_SIZE + TITLEBAR_BUTTON_GAP)
                + TITLEBAR_BUTTON_SIZE / 2.0,
            TITLEBAR_HEIGHT / 2.0,
        )
    };

    app.select_mode = true;
    app.selected_mails
        .insert(MailKey::new("acct", "INBOX", "1"));
    assert!(
        fill_under_the_pointer(
            &ctx,
            &mut app,
            1400.0,
            cell(MailAction::Reply),
            TITLEBAR_BUTTON_SIZE
        )
        .is_none(),
        "a disabled action does not fill under the pointer"
    );
    assert!(
        fill_under_the_pointer(
            &ctx,
            &mut app,
            1400.0,
            cell(MailAction::Delete),
            TITLEBAR_BUTTON_SIZE
        )
        .is_some(),
        "Delete is the one left live in select mode"
    );
}

/// The title bar is laid out as groups, and no group's rect crosses
/// another's — whatever the window's width. That is the whole of why a
/// narrow window no longer stacks the search box and the mail actions on
/// top of the window controls: they used to be one run of widgets that
/// simply drew past the end of the bar.
#[test]
fn the_title_bars_groups_never_cross() {
    let bar = |width: f32| {
        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, TITLEBAR_HEIGHT))
    };
    for width in [
        0.0, 40.0, 126.0, 167.0, 168.0, 169.0, 300.0, 480.0, 640.0, 900.0, 1400.0, 4000.0,
    ] {
        let rects = titlebar_rects(bar(width), 3);
        assert_eq!(
            rects.content.min,
            bar(width).min,
            "{width} pt: the content rect does not start at the bar's own corner"
        );
        assert!(
            rects.content.width() >= 0.0 && rects.content.height() == TITLEBAR_HEIGHT,
            "{width} pt: the content rect is inside out: {rects:?}"
        );
        // No two groups overlap. Where the bar is too narrow to hold the
        // fixed ones at all, `content` is simply empty and the bar shows
        // the sync cell, the cog and the window controls alone.
        for (a, b) in [
            (rects.content, rects.sync),
            (rects.sync, rects.cog),
            (rects.cog, rects.controls),
            (rects.content, rects.controls),
        ] {
            assert!(
                !a.intersect(b).is_positive(),
                "{width} pt: {a:?} overlaps {b:?}"
            );
        }
        // And while there is room for them, they are in that order: what
        // the right end needs is the sync cell, its gap, the cog, its gap,
        // the three controls and the margin from the window's edge.
        let right_end = TITLEBAR_RIGHT_MARGIN
            + TITLEBAR_BUTTON_SIZE
            + TITLEBAR_BUTTON_GAP
            + TITLEBAR_BUTTON_SIZE
            + TITLEBAR_BUTTON_GAP
            + titlebar_cells_width(3);
        if bar(width).width() >= right_end {
            assert!(
                rects.content.right() <= rects.sync.left()
                    && rects.sync.right() <= rects.cog.left()
                    && rects.cog.right() <= rects.controls.left(),
                "{width} pt: the groups are out of order: {rects:?}"
            );
        }
        // The cells keep their size whatever the bar does: a window too
        // narrow to show them all cuts the ones at its edge off rather
        // than shrinking every control into an unclickable sliver.
        assert_eq!(rects.sync.width(), TITLEBAR_BUTTON_SIZE, "{width} pt");
        assert_eq!(rects.cog.width(), TITLEBAR_BUTTON_SIZE, "{width} pt");
        assert_eq!(
            rects.controls.width(),
            titlebar_cells_width(3),
            "{width} pt"
        );
        // The run ends clear of the window's corner rather than on it: the
        // close button's fill is a rounded square now, and flush it would
        // reach the corner the compositor cut.
        assert_eq!(
            rects.controls.right(),
            bar(width).right() - TITLEBAR_RIGHT_MARGIN,
            "{width} pt"
        );
    }
}

/// Every button in the title bar is the same cell: a
/// `TITLEBAR_BUTTON_SIZE` square, centred in the bar's height and
/// `TITLEBAR_BUTTON_GAP` from the next one, whose hover fill is a small
/// rounded square. This walks the whole bar — the left group, the reading
/// pane's run of mail actions, the sync cell, the cog and the window
/// controls — and puts the pointer on each cell in turn, which is also what
/// says every one of them is a real hit target rather than a glyph painted
/// on the chrome.
///
/// The runs are measured with `titlebar_cells_width`, which is what the
/// window controls are placed with and the bar draws the mail actions with;
/// the left group's cells are found from the layout its row makes (the edge
/// margin, then a cell every pitch), and the run of mail
/// actions from `titlebar_anchors`, which is where the bar places it.
#[test]
fn every_button_in_the_title_bar_is_the_same_cell() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 4);
    // The run is enabled by the mail the reading pane shows.
    app.selected_message = Some("0".to_string());

    // One frame first: the run is the reading pane's own row, and the
    // pane's rect is only known once the columns have been laid out.
    frame_shapes_at(&ctx, &mut app, 1400.0);
    let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, TITLEBAR_HEIGHT));
    let rects = titlebar_rects(bar, 3);
    let pitch = TITLEBAR_BUTTON_SIZE + TITLEBAR_BUTTON_GAP;
    let middle = TITLEBAR_HEIGHT / 2.0;

    // The left group: the nav toggle and compose — the window's own
    // actions, a cell every pitch, the first of them the bar's left margin
    // in from the window's edge. Sync is not one of them any more: it
    // stands with the cog at the right end.
    let first_left = TITLEBAR_LEFT_INSET;
    let mut cells: Vec<egui::Pos2> = (0..2)
        .map(|index| {
            egui::pos2(
                first_left + index as f32 * pitch + TITLEBAR_BUTTON_SIZE / 2.0,
                middle,
            )
        })
        .collect();

    // The mail actions: the run is the reading pane's own row, and its
    // leftmost cell starts where the pane does.
    let actions = titlebar_anchors(
        bar,
        first_group_end(),
        app.list_column.left(),
        app.list_column.right(),
        rects.sync.left(),
    )
    .actions
    .expect("the bar gave the actions up at 1400 pt");
    for index in 0..MailAction::ALL.len() {
        cells.push(egui::pos2(
            actions.left() + index as f32 * pitch + TITLEBAR_BUTTON_SIZE / 2.0,
            middle,
        ));
    }

    // The right end: sync, then the cog, then the three window controls,
    // which end `TITLEBAR_RIGHT_MARGIN` short of the window's own edge.
    cells.push(rects.sync.center());
    cells.push(rects.cog.center());
    for index in 0..3 {
        cells.push(egui::pos2(
            rects.controls.right() - TITLEBAR_BUTTON_SIZE / 2.0 - index as f32 * pitch,
            middle,
        ));
    }

    for center in cells {
        let filled = fill_under_the_pointer(&ctx, &mut app, 1400.0, center, TITLEBAR_BUTTON_SIZE)
            .unwrap_or_else(|| panic!("nothing filled under {center:?}"));
        assert_eq!(filled.rect.center(), center);
        assert_eq!(
            filled.corner_radius,
            egui::CornerRadius::same(TITLEBAR_BUTTON_CORNER_RADIUS),
            "{center:?}: the hover fill is not the rounded cell"
        );
    }

    // The run is the width of its cells, and it ends clear of the window
    // controls whatever the window does.
    assert_eq!(actions.width(), titlebar_cells_width(MailAction::ALL.len()));
    assert!(
        actions.right() + MAIL_ACTION_EDGE_GAP <= rects.sync.min.x + 0.5,
        "the run draws {} pt into the bar's right end",
        actions.right() + MAIL_ACTION_EDGE_GAP - rects.sync.min.x
    );
}

/// Minimize is the window manager's by default; only with minimize-to-tray
/// on — and a tray that can bring it back — is the window hidden.
#[test]
fn minimize_to_tray_hides_only_when_a_tray_can_bring_the_window_back() {
    assert_eq!(
        minimize_command(false),
        egui::ViewportCommand::Minimized(true)
    );
    assert_eq!(
        minimize_command(true),
        egui::ViewportCommand::Visible(false)
    );
}

/// Closing quits by default; close-to-tray hides the window only while a
/// tray is registered, so the tray's Quit is the one way out.
#[test]
fn close_to_tray_hides_only_when_a_tray_can_bring_the_window_back() {
    assert_eq!(close_command(false), egui::ViewportCommand::Close);
    assert_eq!(close_command(true), egui::ViewportCommand::Visible(false));
}

/// A click on the minimize cell asks the window manager to minimize when
/// the preference is off — the cell is a real hit target, not a painted
/// glyph. The tray path is `minimize_command`'s above.
#[test]
fn the_minimize_cell_minimizes_when_minimize_to_tray_is_off() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    // One frame first: the right end is carved out of the window's size.
    frame_shapes_at(&ctx, &mut app, 1400.0);
    app.minimize_to_tray = false;

    let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, TITLEBAR_HEIGHT));
    let rects = titlebar_rects(bar, 3);
    let pitch = TITLEBAR_BUTTON_SIZE + TITLEBAR_BUTTON_GAP;
    let center = egui::pos2(
        rects.controls.right() - TITLEBAR_BUTTON_SIZE / 2.0 - 2.0 * pitch,
        TITLEBAR_HEIGHT / 2.0,
    );
    let button = |pressed| egui::Event::PointerButton {
        pos: center,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::default(),
    };

    let mut commands = viewport_commands(&app_frame_output(
        &ctx,
        &mut app,
        vec![egui::Event::PointerMoved(center)],
    ));
    commands.extend(viewport_commands(&app_frame_output(
        &ctx,
        &mut app,
        vec![button(true)],
    )));
    commands.extend(viewport_commands(&app_frame_output(
        &ctx,
        &mut app,
        vec![button(false)],
    )));
    assert!(
        commands.contains(&egui::ViewportCommand::Minimized(true)),
        "the minimize cell asks for the normal minimize: {commands:?}"
    );
}

/// The bar draws no image: it is cells and glyphs, and its leftmost cell is
/// the navigation toggle, the bar's own margin in from the window's edge.
#[test]
fn the_title_bar_draws_no_image() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 2);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, TITLEBAR_HEIGHT));
    let images = image_rects_of(&shapes, bar);
    assert!(images.is_empty(), "the bar draws no image: {images:?}");

    // The toggle's cell is the first thing in the bar, the margin before it
    // being the bar's own.
    let centre = egui::pos2(
        TITLEBAR_LEFT_INSET + TITLEBAR_BUTTON_SIZE / 2.0,
        TITLEBAR_HEIGHT / 2.0,
    );
    assert!(
        fill_under_the_pointer(&ctx, &mut app, 1400.0, centre, TITLEBAR_BUTTON_SIZE).is_some(),
        "the toggle's cell is the bar's first"
    );
}

/// The grips ring the window: one per edge and corner, the corners their
/// own so an edge cannot take them, and none of the window's inside — the
/// columns, the bar, the mail — ever a grip.
#[test]
fn the_resize_grips_ring_the_window() {
    let view = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
    let grips = resize_grips(view);
    let at = |point: egui::Pos2| {
        grips
            .iter()
            .find(|grip| grip.rect.contains(point))
            .map(|grip| grip.direction)
    };

    // The corners, where two edges meet, answer for themselves.
    assert_eq!(
        at(egui::pos2(1.0, 1.0)),
        Some(egui::viewport::ResizeDirection::NorthWest)
    );
    assert_eq!(
        at(egui::pos2(view.right() - 1.0, 1.0)),
        Some(egui::viewport::ResizeDirection::NorthEast)
    );
    assert_eq!(
        at(egui::pos2(1.0, view.bottom() - 1.0)),
        Some(egui::viewport::ResizeDirection::SouthWest)
    );
    assert_eq!(
        at(egui::pos2(view.right() - 1.0, view.bottom() - 1.0)),
        Some(egui::viewport::ResizeDirection::SouthEast)
    );
    // And the edges for the rest of their length.
    assert_eq!(
        at(egui::pos2(view.center().x, 1.0)),
        Some(egui::viewport::ResizeDirection::North)
    );
    assert_eq!(
        at(egui::pos2(view.center().x, view.bottom() - 1.0)),
        Some(egui::viewport::ResizeDirection::South)
    );
    assert_eq!(
        at(egui::pos2(1.0, view.center().y)),
        Some(egui::viewport::ResizeDirection::West)
    );
    assert_eq!(
        at(egui::pos2(view.right() - 1.0, view.center().y)),
        Some(egui::viewport::ResizeDirection::East)
    );

    // Nothing of the window's inside is a grip: the border is a border,
    // however thick.
    let inside = view.shrink(RESIZE_GRIP + 0.01);
    assert!(
        grips.iter().all(|grip| !grip.rect.intersects(inside)),
        "a grip reaches into the window: {grips:?}"
    );
}

/// The undecorated window's own resize border, at the frame level: a press
/// on an edge asks the compositor to resize from that edge (`BeginResize`),
/// the pointer over one wears that edge's cursor, and the inside of the
/// window asks for nothing.
#[test]
fn the_window_edges_ask_the_compositor_to_resize() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 2);
    let view = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
    // One frame first, so the grips are among the rects the frames below
    // are hit-tested against.
    let _ = app_frame_output(&ctx, &mut app, Vec::new());

    let half = RESIZE_GRIP / 2.0;
    let button = |point: egui::Pos2, pressed: bool| egui::Event::PointerButton {
        pos: point,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::default(),
    };
    let cases = [
        (
            egui::pos2(view.left() + half, view.center().y),
            egui::viewport::ResizeDirection::West,
        ),
        (
            egui::pos2(view.right() - half, view.center().y),
            egui::viewport::ResizeDirection::East,
        ),
        (
            egui::pos2(view.center().x, view.top() + half),
            egui::viewport::ResizeDirection::North,
        ),
        (
            egui::pos2(view.center().x, view.bottom() - half),
            egui::viewport::ResizeDirection::South,
        ),
        (
            egui::pos2(view.left() + half, view.top() + half),
            egui::viewport::ResizeDirection::NorthWest,
        ),
        (
            egui::pos2(view.right() - half, view.bottom() - half),
            egui::viewport::ResizeDirection::SouthEast,
        ),
    ];
    for (point, direction) in cases {
        // The frame that puts the pointer on the grip, then the one that
        // presses it, then the release.
        let _ = app_frame_output(&ctx, &mut app, vec![egui::Event::PointerMoved(point)]);
        let pressed = app_frame_output(&ctx, &mut app, vec![button(point, true)]);
        assert!(
            viewport_commands(&pressed).contains(&egui::ViewportCommand::BeginResize(direction)),
            "{point:?} is the {direction:?} grip: {:?}",
            viewport_commands(&pressed)
        );
        let _ = app_frame_output(&ctx, &mut app, vec![button(point, false)]);
    }

    // The cursor says which grip before the press does.
    let moved = app_frame_output(&ctx, &mut app, vec![egui::Event::PointerMoved(cases[0].0)]);
    assert_eq!(
        moved.platform_output.cursor_icon,
        egui::CursorIcon::ResizeWest
    );

    // And the window's own inside is nobody's grip — neither the middle of
    // the window nor the title bar just under the top strip, where the bar's
    // own drag stretches still move it.
    for point in [
        view.center(),
        egui::pos2(view.center().x, RESIZE_GRIP + 5.0),
    ] {
        let _ = app_frame_output(&ctx, &mut app, vec![egui::Event::PointerMoved(point)]);
        let inside = app_frame_output(&ctx, &mut app, vec![button(point, true)]);
        assert!(
            !viewport_commands(&inside)
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::BeginResize(_))),
            "{point:?} is not a grip: {:?}",
            viewport_commands(&inside)
        );
        let _ = app_frame_output(&ctx, &mut app, vec![button(point, false)]);
    }
}

/// The one thing the bar gives up when the room runs short is the search
/// box's width, and then the box itself: its own width while there is room
/// for it, its floor while the room is short, nothing below that — and never
/// narrower for a wider bar, which is what keeps a window being resized from
/// flickering between two shapes.
#[test]
fn the_title_bar_gives_its_room_up_in_one_order() {
    let widest = TITLEBAR_SEARCH_WIDTH;
    let narrowest = TITLEBAR_SEARCH_FLOOR;

    assert_eq!(titlebar_search_width(4000.0), Some(widest));
    assert_eq!(titlebar_search_width(widest), Some(widest));
    assert_eq!(titlebar_search_width(widest - 1.0), Some(widest - 1.0));
    assert_eq!(
        titlebar_search_width(narrowest + 30.0),
        Some(narrowest + 30.0)
    );
    assert_eq!(titlebar_search_width(narrowest), Some(narrowest));
    assert_eq!(titlebar_search_width(narrowest - 1.0), None);

    let mut previous = 0.0;
    for step in 0..6000 {
        let room = step as f32 / 4.0;
        let width = titlebar_search_width(room);
        if let Some(width) = width {
            assert!(
                (narrowest..=widest).contains(&width),
                "at {room} pt the box is not a box: {width}"
            );
            assert!(
                width <= room + 0.01,
                "at {room} pt the box is wider than the room: {width}"
            );
            assert!(
                width >= previous,
                "at {room} pt a wider bar holds a narrower box: {width} after {previous}"
            );
            previous = width;
        } else {
            assert_eq!(previous, 0.0, "at {room} pt the box came back");
        }
    }
}

/// The bar draws its groups in the rects `titlebar_rects` carves out of
/// it — every group clipped to its own — so nothing it draws is ever in
/// the window controls' half of the bar, whatever the window's width. This
/// is the frame-level half of the two tests above: the arithmetic can be
/// right while the bar draws from the window's left edge anyway.
///
/// The shapes are sorted by the clip they were drawn through, which is the
/// group rect itself, so the test can tell the bar's right end from a
/// widget that has wandered into it — which is exactly what used to
/// happen, when the bar was one run of widgets that drew past the end of
/// the window and the mail cells landed on minimize / maximize / close.
#[test]
fn nothing_in_the_title_bar_is_drawn_into_the_window_controls() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 4);

    for width in [1400.0, 1000.0, 900.0, 800.0, 560.0, 480.0, 400.0, 300.0] {
        let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, TITLEBAR_HEIGHT));
        let rects = titlebar_rects(bar, 3);
        // The bar's own right end: the sync cell, the cog and the window
        // controls, from the sync cell's left edge to the window's corner.
        // That is the rect the right group draws through — its cells are
        // `TITLEBAR_BUTTON_SIZE` squares inside it — so a shape clipped to it
        // is the group's own, and anything else that reaches into it is what
        // this catches.
        let right_end = egui::Rect::from_min_max(egui::pos2(rects.sync.left(), bar.top()), bar.max);
        for clipped in frame_shapes_at(&ctx, &mut app, width) {
            // What the frame actually shows of the shape: the bar clips
            // its groups, and egui clips the window.
            let visible = clipped
                .shape
                .visual_bounding_rect()
                .intersect(clipped.clip_rect);
            if !visible.is_positive() {
                continue;
            }
            // Only what lives in the bar's own band, so that the columns
            // the bar floats over — taller than it, and drawn first — are
            // not mistaken for something in the bar...
            if visible.top() < bar.top() - 0.5 || visible.bottom() > bar.bottom() + 0.5 {
                continue;
            }
            // ...and the bar's own fill, which spans it by design.
            if visible.width() >= bar.width() - 1.0 {
                continue;
            }
            if right_end.contains_rect(clipped.clip_rect) {
                continue;
            }
            assert!(
                visible.right() <= rects.sync.left() + 0.5,
                "at {width} pt the bar drew {visible:?} into the window controls, which \
                     start at {}: it was drawn through {clip:?}",
                rects.sync.left(),
                clip = clipped.clip_rect
            );
        }
    }
}

/// A window too short for the card is the *pane's* to scroll: the writing
/// area keeps `COMPOSE_BODY_HEIGHT` rather than being squeezed into the room
/// left under the To row, and the card's own surface covers all of it — the
/// card runs past the pane's floor, and the pane's bar is what reaches the
/// rest. The area is claimed outright (`compose_body`'s `scope_builder`), so
/// a pane a few hundred points tall does not leave a viewport of two lines
/// over a box `COMPOSE_BODY_HEIGHT` tall.
#[test]
fn the_answers_body_keeps_its_height_in_a_short_window() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.selected_message = Some("0".to_string());
    let short = egui::vec2(1400.0, 320.0);
    run_app_frame_sized(&ctx, &mut app, Vec::new(), short);

    // The reply cell, clicked at that window size: moved onto, down, up.
    let cell = header_action_center(4);
    run_app_frame_sized(&ctx, &mut app, vec![egui::Event::PointerMoved(cell)], short);
    for pressed in [true, false] {
        run_app_frame_sized(
            &ctx,
            &mut app,
            vec![egui::Event::PointerButton {
                pos: cell,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            }],
            short,
        );
    }
    for _ in 0..3 {
        run_app_frame_sized(&ctx, &mut app, Vec::new(), short);
    }

    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, short)),
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| app.ui(ui));
    output.textures_delta.clear();
    let scheme = theme::of(&ctx);
    let body = *fills_in_color(&output.shapes, scheme.input)
        .iter()
        .find(|rect| rect.center().x > app.list_column.right())
        .expect("the body is on the pane");
    assert!(
        (body.height() - COMPOSE_BODY_HEIGHT).abs() < 1.0,
        "the writing area keeps its height: {body:?}"
    );
    let card = *fills_in_color(&output.shapes, scheme.content)
        .iter()
        .find(|rect| rect.contains(body.center()))
        .expect("the answer's card is under the body");
    assert!(
        card.contains_rect(body),
        "the card's surface covers the writing area: {card:?} of {body:?}"
    );
}
