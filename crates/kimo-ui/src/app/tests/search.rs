use super::super::*;
use super::common::*;

/// A search's hits are selectable too, and a hit names the folder it lives
/// in: the key its box makes is the hit's own account and folder, not the
/// column's — the column is not on a folder at all while it shows hits.
#[test]
fn a_search_hit_checks_its_own_folder() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.search.text = "report".to_string();
    app.search_hits = vec![SearchHit {
        account_id: "acct".to_string(),
        folder: "Archive".to_string(),
        uid: "7".to_string(),
        subject: "Subject 7".to_string(),
        from: "Ada <ada@example.com>".to_string(),
        to: "me@example.com".to_string(),
        date: "2026-01-01".to_string(),
        timestamp: Some(1_700_000_000),
        seen: false,
        flagged: false,
        attachments: 0,
        snippet: "a line with report in it".to_string(),
    }];
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

    let at = checkbox_center(&app, 0, false);
    click_at(&ctx, &mut app, at);
    assert!(
        app.selected_mails
            .contains(&MailKey::new("acct", "Archive", "7")),
        "the hit's box checks it where it lives: {:?}",
        app.selected_mails
    );
}

/// The search results have the row too, and a hit names the folder it lives
/// in: selecting them all keys them where they live, not in the column.
#[test]
fn select_all_in_the_search_results_checks_every_hit() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.search.text = "report".to_string();
    app.search_hits = vec![
        SearchHit {
            account_id: "acct".to_string(),
            folder: "INBOX".to_string(),
            uid: "0".to_string(),
            subject: "Subject 0".to_string(),
            from: "Ada <ada@example.com>".to_string(),
            date: "2026-01-01".to_string(),
            timestamp: Some(1_700_000_000),
            seen: false,
            flagged: false,
            to: "me@example.com".to_string(),
            attachments: 0,
            snippet: "a line with report in it".to_string(),
        },
        SearchHit {
            account_id: "acct".to_string(),
            folder: "Archive".to_string(),
            uid: "7".to_string(),
            subject: "Subject 7".to_string(),
            from: "Ada <ada@example.com>".to_string(),
            date: "2026-01-01".to_string(),
            timestamp: Some(1_700_000_000),
            seen: false,
            flagged: false,
            to: "me@example.com".to_string(),
            attachments: 0,
            snippet: "another line with report in it".to_string(),
        },
        SearchHit {
            account_id: "home".to_string(),
            folder: "INBOX".to_string(),
            uid: "2".to_string(),
            subject: "Subject 2".to_string(),
            from: "Ada <ada@example.com>".to_string(),
            date: "2026-01-01".to_string(),
            timestamp: Some(1_700_000_000),
            seen: false,
            flagged: false,
            to: "me@example.com".to_string(),
            attachments: 0,
            snippet: "a third line with report in it".to_string(),
        },
    ];
    app.select_mode = true;
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

    let at = select_all_center(&app);
    click_at(&ctx, &mut app, at);
    assert_eq!(app.selected_mails.len(), 3, "every hit, across folders");
    for (account, folder, uid) in [
        ("acct", "INBOX", "0"),
        ("acct", "Archive", "7"),
        ("home", "INBOX", "2"),
    ] {
        assert!(
            app.selected_mails
                .contains(&MailKey::new(account, folder, uid)),
            "{account}/{folder}/{uid} is checked"
        );
    }
}

/// A fresh answer is the whole of what the column shows, so a checked hit
/// the answer no longer names is dropped with it — the band counts the
/// selection against the hits and must not be able to count a mail that is
/// not there. An answer for a query already cleared says nothing about a
/// folder's checks.
#[test]
fn a_new_search_drops_checks_the_hits_no_longer_hold() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 2);
    app.search.text = "report".to_string();
    app.select_mode = true;
    for uid in ["0", "1"] {
        app.selected_mails
            .insert(MailKey::new("acct", "INBOX", uid));
    }

    // An answer that names one of the two: the other is not a hit any more.
    app.apply_event(Event::SearchResults {
        seq: app.search_seq,
        hits: vec![SearchHit {
            account_id: "acct".to_string(),
            folder: "INBOX".to_string(),
            uid: "0".to_string(),
            subject: "Subject 0".to_string(),
            from: "Ada <ada@example.com>".to_string(),
            date: "2026-01-01".to_string(),
            timestamp: Some(1_700_000_000),
            seen: false,
            flagged: false,
            to: "me@example.com".to_string(),
            attachments: 0,
            snippet: "a line with report in it".to_string(),
        }],
    });
    assert_eq!(app.selected_mails.len(), 1, "the answer's own hit");
    assert!(app
        .selected_mails
        .contains(&MailKey::new("acct", "INBOX", "0")));

    // Cleared query: the same event describes hits nobody is looking at.
    app.search = SearchQuery::default();
    app.selected_mails
        .insert(MailKey::new("acct", "INBOX", "1"));
    app.apply_event(Event::SearchResults {
        seq: app.search_seq,
        hits: Vec::new(),
    });
    assert_eq!(
        app.selected_mails.len(),
        2,
        "the folder's checks are not the answer's to drop"
    );
}

/// A hit's own delete button is keyed by the hit and not by the row: the
/// click is not the row's, so the window does not switch to the folder the
/// hit lives in.
#[test]
fn a_search_hits_delete_button_is_not_its_row() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.search.text = "report".to_string();
    app.search_hits = vec![SearchHit {
        account_id: "acct".to_string(),
        folder: "Archive".to_string(),
        uid: "7".to_string(),
        subject: "Subject 7".to_string(),
        from: "Ada <ada@example.com>".to_string(),
        to: "me@example.com".to_string(),
        date: "2026-01-01".to_string(),
        timestamp: Some(1_700_000_000),
        seen: false,
        flagged: false,
        attachments: 0,
        snippet: "a line with report in it".to_string(),
    }];
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

    let at = row_delete_center(&app, 0);
    click_at(&ctx, &mut app, at);
    assert_eq!(
        app.selected_folder.as_deref(),
        Some("INBOX"),
        "the click deleted rather than opened the hit"
    );
    assert_eq!(app.selected_message, None);
}

/// The box the bar plans for and the box it draws are the same box:
/// `titlebar_search_width`'s answer is what the widget comes to, margins and
/// all, and it starts at the message list's own left edge. Two pieces of
/// arithmetic decide that — the plan's constants and egui's
/// `desired_width` — and they are not the same arithmetic, so this is what
/// keeps them from drifting. (`desired_width` is the *whole* box: the margins
/// come out of it, they are not added to it.)
#[test]
fn the_search_box_is_as_wide_as_it_was_planned() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 4);

    // A window with room for the whole bar: the box is at its own width.
    frame_shapes_at(&ctx, &mut app, 1400.0);
    let drawn = ctx
        .read_response(egui::Id::new("titlebar-search"))
        .expect("the bar drew no search box at 1400 pt")
        .rect;
    let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, TITLEBAR_HEIGHT));
    let rects = titlebar_rects(bar, 3);
    let pane_left = app.list_column.right();
    let anchors = titlebar_anchors(
        bar,
        first_group_end(),
        app.list_column.left(),
        pane_left,
        rects.sync.left(),
    );
    let next = anchors
        .actions
        .map_or(rects.sync.left(), |actions| actions.left());
    let planned = titlebar_search_width(next - anchors.search - TITLEBAR_GROUP_GAP)
        .expect("the bar planned no box at 1400 pt");
    assert!(
        (drawn.width() - planned).abs() < 0.5,
        "the bar plans a {planned} pt box and drew a {} pt one",
        drawn.width()
    );
    assert!(
        (drawn.left() - anchors.search).abs() < 0.5,
        "the box starts at the list's own edge: {drawn:?} against {}",
        anchors.search
    );
}

/// The search box is the message list's: it starts where the list starts and
/// follows the splitter that moves the list's left edge — and when the
/// navigation column is collapsed and the list runs to the window's own edge,
/// it stops clear of the first group instead of running under it.
#[test]
fn the_search_box_follows_the_list() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 4);

    // A wider navigation column moves the list's left edge, and the box
    // with it.
    set_panel_width(&ctx, "nav", 260.0);
    run_app_frame(&ctx, &mut app, Vec::new());
    run_app_frame(&ctx, &mut app, Vec::new());
    let drawn = ctx
        .read_response(egui::Id::new("titlebar-search"))
        .expect("the bar drew no search box")
        .rect;
    assert!(
        (drawn.left() - app.list_column.left()).abs() < 1.0,
        "the box is at the list's left edge: {drawn:?} against {:?}",
        app.list_column
    );

    // Collapsed: the navigation slides out, the list takes the room over,
    // and the box follows it — never onto the first group.
    app.nav_collapsed = true;
    for _ in 0..16 {
        run_app_frame(&ctx, &mut app, Vec::new());
        let drawn = ctx
            .read_response(egui::Id::new("titlebar-search"))
            .expect("the bar drew no search box");
        assert!(
            drawn.rect.left() >= first_group_end() + TITLEBAR_GROUP_GAP - 0.5,
            "the box stays clear of the first group: {:?}",
            drawn.rect
        );
    }
}

/// The search popup is the bar and the form under it: no heading, no close
/// button and no Search button — the query runs as it is typed — with the
/// magnifier drawn inside the bar's left edge and the round cross inside its
/// right once there is something to clear.
#[test]
fn the_search_popup_is_a_bar_with_a_magnifier_and_a_clear_cross() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.open_search();
    for _ in 0..FADE_FRAMES {
        run_app_frame(&ctx, &mut app, Vec::new());
    }

    // Nothing of the old chrome is drawn: the popup says what it is by
    // being a search bar, and Escape or a click outside closes it.
    let texts = frame_texts(&ctx, &mut app);
    for gone in ["Search", "Close", "Clear"] {
        assert!(
            !texts.iter().any(|text| text == gone),
            "{gone} is not drawn any more: {texts:?}"
        );
    }
    for still in ["Search in", "Subject", "Only"] {
        assert!(
            texts.iter().any(|text| text == still),
            "{still} is still on the form: {texts:?}"
        );
    }

    // The bar is a box of its own height, and the magnifier's circle sits
    // inside its left edge (the painter centres it a point up and left of
    // the rect it is given).
    let bar_of = |shapes: &[egui::epaint::ClippedShape]| -> egui::Rect {
        stroked_boxes_in(shapes, popup_area())
            .into_iter()
            .map(|(rect, _)| rect)
            .find(|rect| (rect.height() - SEARCH_BAR_HEIGHT).abs() < 0.5)
            .expect("the search bar is a field with a box")
    };
    let cross_of =
        |bar: egui::Rect| egui::pos2(bar.right() - SEARCH_ICON_INSET as f32 / 2.0, bar.center().y);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let bar = bar_of(&shapes);
    let magnifier = egui::pos2(
        bar.left() + SEARCH_ICON_INSET as f32 / 2.0 - 1.0,
        bar.center().y - 1.0,
    );
    assert!(
        stroked_circles_in(&shapes, popup_area())
            .iter()
            .any(|centre| centre.distance(magnifier) < 1.0),
        "the magnifier is drawn inside the bar's left edge"
    );

    // The hint keeps clear of the magnifier: where the text starts inside
    // the box is the field's own margin (`Field::margin`), which is what
    // `TextEdit::margin` cannot say once `Field` gives the widget a frame
    // of its own.
    let hint = text_boxes_of(&shapes, popup_area(), "Search all mail…")
        .into_iter()
        .next()
        .expect("the bar's hint");
    assert!(
        hint.left() >= bar.left() + SEARCH_ICON_INSET as f32,
        "the text starts after the magnifier: {hint:?} in {bar:?}"
    );

    // The scope is a flat combo: no box at rest, and the app's chevron
    // where egui would put its filled triangle.
    let scope = text_boxes_of(&shapes, popup_area(), "All mail")
        .into_iter()
        .next()
        .expect("the scope's own text");
    let scope_row = egui::Rect::from_min_max(
        egui::pos2(scope.left() - 60.0, scope.top() - 4.0),
        egui::pos2(scope.left() + 220.0, scope.bottom() + 4.0),
    );
    assert!(
        triangles_in(&shapes, scope_row).is_empty(),
        "the scope keeps no filled triangle"
    );
    assert!(
        !chevrons_in(&shapes, scope_row).is_empty(),
        "it ends with the app's chevron instead"
    );

    // An empty bar has nothing to clear, so no cross.
    assert!(
        !stroke_centres(&shapes, popup_area())
            .iter()
            .any(|centre| centre.distance(cross_of(bar)) < 1.0),
        "an empty bar has no cross"
    );

    // Typed in, the cross is there — two strokes meeting at its centre — and
    // a click empties the bar without closing the popup. The bar is found
    // again rather than reused, and only once the form has settled: a query
    // gives the popup the row its count stands on, which moves the whole
    // form up with it by the time the next frame is drawn.
    app.search.text = "ada".to_string();
    for _ in 0..3 {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let bar = bar_of(&shapes);
    let clear = cross_of(bar);
    assert_eq!(
        stroke_centres(&shapes, popup_area())
            .iter()
            .filter(|centre| centre.distance(clear) < 1.0)
            .count(),
        2,
        "the cross is two strokes in the bar's right end"
    );
    click_at(&ctx, &mut app, clear);
    assert!(app.search.text.is_empty(), "the cross emptied the bar");
    assert!(app.show_search, "and the popup stayed open");
}

#[test]
fn a_search_waits_for_the_typing_to_stop() {
    let start = Instant::now();
    let typed = SearchQuery {
        text: "invoice".to_string(),
        ..SearchQuery::default()
    };
    // The first keystroke starts the clock…
    assert_eq!(search_settle(None, &typed, start), SearchWait::Reset);
    // …which then waits…
    assert_eq!(
        search_settle(Some((&typed, start)), &typed, start + SEARCH_SETTLE / 2),
        SearchWait::Wait
    );
    // …until the query has held still.
    assert_eq!(
        search_settle(Some((&typed, start)), &typed, start + SEARCH_SETTLE),
        SearchWait::Run
    );
    // Another keystroke starts a new clock instead of running the old query.
    let longer = SearchQuery {
        text: "invoices".to_string(),
        ..SearchQuery::default()
    };
    assert_eq!(
        search_settle(Some((&typed, start)), &longer, start + SEARCH_SETTLE),
        SearchWait::Reset
    );
}
