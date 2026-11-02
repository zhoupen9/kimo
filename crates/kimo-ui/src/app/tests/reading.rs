use super::super::*;
use super::common::*;

#[test]
fn blitz_keys_depend_on_width_but_not_pane_height() {
    let identity = "acct/INBOX/1/";
    let key = blitz_key(identity, 864, 2.0);
    // Nothing else feeds the key, so a pane that only grew taller keeps
    // its page (the height is not part of the key at all).
    assert_eq!(key, blitz_key(identity, 864, 2.0));
    // A new width, or a new display scale, is a different raster.
    assert_ne!(key, blitz_key(identity, 896, 2.0));
    assert_ne!(key, blitz_key(identity, 864, 1.0));
    // So is another mail.
    assert_ne!(key, blitz_key("acct/INBOX/2/", 864, 2.0));
    assert_ne!(key, blitz_key("acct/INBOX/18/", 864, 2.0));
}

#[test]
fn blitz_renders_wait_for_the_geometry_to_settle() {
    let start = Instant::now();
    // Never seen before: nothing renders on the first frame.
    assert_eq!(
        blitz_settle(None, "page", start),
        BlitzWait::Wait(GEOMETRY_SETTLE)
    );
    // Still settling: report what is left of the wait.
    assert_eq!(
        blitz_settle(
            Some(("page", start)),
            "page",
            start + Duration::from_millis(100)
        ),
        BlitzWait::Wait(GEOMETRY_SETTLE - Duration::from_millis(100))
    );
    // Held still long enough: render.
    assert_eq!(
        blitz_settle(Some(("page", start)), "page", start + GEOMETRY_SETTLE),
        BlitzWait::Start
    );
    // A geometry that changed again starts a fresh wait, however long the
    // previous one had been pending.
    assert_eq!(
        blitz_settle(
            Some(("page", start)),
            "page-moved",
            start + GEOMETRY_SETTLE * 4
        ),
        BlitzWait::Wait(GEOMETRY_SETTLE)
    );
}

/// Below the two minimums side by side, the reading pane stops being a
/// column: the list has the window, a click on a mail opens the pane over
/// it, and the band under the title bar is the way back to the list.
#[test]
fn the_reading_pane_becomes_a_popup_over_the_list() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    for _ in 0..10 {
        run_app_frame_sized(&ctx, &mut app, Vec::new(), egui::vec2(500.0, 900.0));
    }
    assert!(app.pane_popup);
    assert!(app.nav_collapsed, "the navigation gave way long before");
    // The list has the whole window — the pane over it is an overlay, not
    // a column taking part of it.
    assert!((app.list_column.width() - 500.0).abs() < 0.5);

    // A click on a mail opens it, over the list and under the title bar,
    // and the list's own rect is unchanged: nothing was taken from it.
    click_at_sized(&ctx, &mut app, egui::pos2(250.0, 74.0), 500.0);
    assert_eq!(app.selected_message.as_deref(), Some("0"));
    assert!((app.list_column.width() - 500.0).abs() < 0.5);

    // The way back is a round button floating over the mail's top-left
    // corner — a circle where the plan puts it, and a blurred shape
    // behind it: the shadow is what makes it read as floating rather
    // than as a hole in the mail.
    let shapes = frame_shapes_at(&ctx, &mut app, 500.0);
    let centre = egui::pos2(
        POPUP_BACK_MARGIN + POPUP_BACK_DIAMETER / 2.0,
        TITLEBAR_HEIGHT + POPUP_BACK_MARGIN + POPUP_BACK_DIAMETER / 2.0,
    );
    assert!(
        shapes.iter().any(|clipped| match &clipped.shape {
            egui::Shape::Circle(circle) =>
                circle.center.distance(centre) < 1.0
                    && (circle.radius - POPUP_BACK_DIAMETER / 2.0).abs() < 1.0,
            _ => false,
        }),
        "the back button is a circle where the popup puts it"
    );
    let button = egui::Rect::from_center_size(centre, egui::Vec2::splat(POPUP_BACK_DIAMETER));
    assert!(
        shapes.iter().any(|clipped| match &clipped.shape {
            egui::Shape::Rect(shadow) => shadow.blur_width > 0.0 && shadow.rect.intersects(button),
            _ => false,
        }),
        "a blurred shape behind the button is its shadow"
    );

    // A click on it closes the mail, and the list is what is on screen
    // again.
    click_at_sized(&ctx, &mut app, centre, 500.0);
    assert!(
        app.selected_message.is_none(),
        "the way back closed the mail"
    );

    // Narrower than the list's own minimum, the list keeps that minimum
    // and scrolls sideways instead of crushing its rows.
    for _ in 0..5 {
        run_app_frame_sized(&ctx, &mut app, Vec::new(), egui::vec2(200.0, 900.0));
    }
    assert!(app.list_scrolls);
    assert!(app.pane_popup);
}

/// Every attachment is a card under the body: the file's name, and the type
/// and size the mail was fetched with under it. A mail cached before the
/// database kept those two is the name alone — no second line, and no
/// separator left standing on its own.
#[test]
fn every_attachment_is_a_card_under_the_body() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.states
        .get_mut("acct")
        .expect("the fixture account")
        .mailbox
        .folders[0]
        .messages[0]
        .attachments = vec![
        Attachment {
            name: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size: 68 * 1024,
        },
        Attachment {
            name: "cached-before-v6.png".to_string(),
            content_type: String::new(),
            size: 0,
        },
    ];
    app.selected_account = Some("acct".to_string());
    app.selected_folder = Some("INBOX".to_string());
    app.selected_message = Some("0".to_string());

    // The first frame is what sizes the pane; the mail is drawn from the
    // second one on, the way the app draws it live.
    let _ = frame_texts(&ctx, &mut app);
    let texts = frame_texts(&ctx, &mut app);

    let position = |text: &str| {
        texts
            .iter()
            .position(|drawn| drawn == text)
            .unwrap_or_else(|| panic!("{text:?} is not on the card: {texts:?}"))
    };
    let body = position("body");
    assert!(
        body < position("report.pdf"),
        "the cards come after the body: {texts:?}"
    );
    assert!(
        position("application/pdf · 68 KB") < position("cached-before-v6.png"),
        "the cards keep the order the parts had: {texts:?}"
    );
    assert!(
        !texts.iter().any(|text| text.starts_with('·')),
        "the old row left a separator behind: {texts:?}"
    );

    // The names are links: drawn in the scheme's link color rather than in
    // the body text's, which is what says a click is offered.
    let link = theme::of(&ctx).link;
    let colors = frame_text_colors(&ctx, &mut app);
    for name in ["report.pdf", "cached-before-v6.png"] {
        assert!(
            colors
                .iter()
                .any(|(drawn, color)| drawn == name && *color == link),
            "{name} is not drawn as a link: {colors:?}"
        );
    }
}

/// The attachment cards are one run across the mail's column, not one card
/// per line: a fixed width each, two to a row where they fit, the third
/// wrapped onto the next row at the column's own left edge.
#[test]
fn the_attachment_cards_wrap_across_the_mails_column() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    let part = |name: &str| Attachment {
        name: name.to_string(),
        content_type: "application/pdf".to_string(),
        size: 68 * 1024,
    };
    // Names long enough that three cards do not fit in the wider reading
    // pane: two go on the first row and the third wraps.
    let three = vec![
        part("a-long-attachment-name-for-a-report-one.pdf"),
        part("a-long-attachment-name-for-a-report-two.png"),
        part("a-long-attachment-name-for-a-report-three.pdf"),
    ];
    app.states
        .get_mut("acct")
        .expect("the fixture account")
        .mailbox
        .folders[0]
        .messages[0]
        .attachments = three.clone();
    app.selected_account = Some("acct".to_string());
    app.selected_folder = Some("INBOX".to_string());
    app.selected_message = Some("0".to_string());

    // The pane sizes the first frame; the cards are on screen from the
    // second one on, the way the app draws them live.
    let _ = frame_texts(&ctx, &mut app);
    let cards = attachment_cards_in(&ctx, &mut app);
    assert_eq!(cards.len(), three.len(), "one card per part: {cards:?}");
    for card in &cards {
        assert!(
            card.width() >= ATTACHMENT_CARD_MIN_WIDTH - 0.5,
            "a card is at least the floor wide, whatever it holds: {cards:?}"
        );
    }
    // The cards sharing a top edge are one row, in the order they were laid
    // out.
    let mut rows: Vec<Vec<egui::Rect>> = Vec::new();
    for card in cards {
        match rows.last_mut() {
            Some(row) if (row[0].top() - card.top()).abs() < 0.5 => row.push(card),
            _ => rows.push(vec![card]),
        }
    }
    assert_eq!(rows.len(), 2, "three cards, two rows: {rows:?}");
    assert_eq!(rows[0].len(), 2, "two of them fit side by side: {rows:?}");
    assert_eq!(rows[1].len(), 1, "the third one wraps: {rows:?}");
    let between = rows[0][1].left() - rows[0][0].right();
    assert!(
        (between - ATTACHMENT_CARD_GAP).abs() < 0.5,
        "two cards are {between} pt apart: {rows:?}"
    );
    assert!(
        (rows[1][0].left() - rows[0][0].left()).abs() < 0.5,
        "the wrapped row starts at the column's own left edge: {rows:?}"
    );
    let drop = rows[1][0].top() - rows[0][0].bottom();
    assert!(
        (drop - ATTACHMENT_CARD_GAP).abs() < 0.5,
        "the wrapped row is {drop} pt under the one above: {rows:?}"
    );

    // Two parts fit on one row: nothing wraps while there is room.
    app.states
        .get_mut("acct")
        .expect("the fixture account")
        .mailbox
        .folders[0]
        .messages[0]
        .attachments = three[..2].to_vec();
    let _ = frame_texts(&ctx, &mut app);
    let cards = attachment_cards_in(&ctx, &mut app);
    assert_eq!(cards.len(), 2, "{cards:?}");
    assert!(
        (cards[0].top() - cards[1].top()).abs() < 0.5,
        "the two share a row: {cards:?}"
    );
}

/// A card is as wide as the file's name wants rather than a fixed width, and
/// the name is drawn whole: the ellipsis is only for a name the mail's own
/// column cannot hold.
#[test]
fn an_attachment_card_shows_the_whole_name() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    let part = |name: &str| Attachment {
        name: name.to_string(),
        content_type: "application/pdf".to_string(),
        size: 68 * 1024,
    };
    let long = "quarterly-report-2026-with-notes.pdf";
    app.states
        .get_mut("acct")
        .expect("the fixture account")
        .mailbox
        .folders[0]
        .messages[0]
        .attachments = vec![part("chart.png"), part(long)];
    app.selected_account = Some("acct".to_string());
    app.selected_folder = Some("INBOX".to_string());
    app.selected_message = Some("0".to_string());

    // The pane sizes the first frame; the cards are on screen from the
    // second one on, the way the app draws them live.
    let _ = frame_texts(&ctx, &mut app);
    let texts = frame_texts(&ctx, &mut app);
    // Both names are drawn verbatim: a cut one would end in the ellipsis
    // instead, and no text of either name would be there to find.
    for name in ["chart.png", long] {
        assert!(
            texts.iter().any(|text| text == name),
            "{name} is on its card whole: {texts:?}"
        );
    }

    let cards = attachment_cards_in(&ctx, &mut app);
    assert_eq!(cards.len(), 2, "one card per part: {cards:?}");
    let widest = cards.iter().map(|card| card.width()).fold(0.0, f32::max);
    let narrowest = cards
        .iter()
        .map(|card| card.width())
        .fold(f32::MAX, f32::min);
    assert!(
        widest > narrowest + 20.0,
        "the name is what a card is as wide as: {cards:?}"
    );

    // No cross on any of them: a mail's own attachment is not Kimo's to take
    // away, so the reading pane's cards are the ones the compose form's are
    // shaped like and nothing more (`attachment_card`).
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let area = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
    let strokes = stroke_centres(&shapes, area);
    for card in &cards {
        assert!(
            !strokes.iter().any(|centre| card.contains(*centre)),
            "the pane's cards carry no button: {card:?} {strokes:?}"
        );
    }
}

/// A right-click on a link in the body offers the link's own actions, and
/// Copy link address puts the href on the clipboard. The menu is drawn
/// before the body on the next frame, which is also what lets a Copy of a
/// selection run before the menu's click clears it.
#[test]
fn a_body_link_offers_its_address_on_right_click() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    {
        let state = app.states.get_mut("acct").expect("the fixture account");
        let mail = &mut state.mailbox.folders[0].messages[0];
        mail.body_text = "Hello Example".to_string();
        mail.body_html =
            Some("<p>Hello <a href=\"https://example.com/a\">Example</a></p>".to_string());
        state.folders_changed();
    }
    app.selected_message = Some("0".to_string());
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let window = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
    let link = text_boxes_of(&shapes, window, "Example")
        .into_iter()
        .next()
        .expect("the body draws its link");

    right_click_at(&ctx, &mut app, link.center());
    assert!(app.body_menu.is_some(), "the right-click reached the body");
    // The first frame is the popup's sizing pass; the menu is painted on
    // the one after it.
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    for item in [
        "Open link",
        "Copy link address",
        "Copy",
        "Copy message text",
    ] {
        assert!(
            !text_boxes_of(&shapes, window, item).is_empty(),
            "the body menu draws {item}"
        );
    }
    let copy = text_boxes_of(&shapes, window, "Copy link address")
        .into_iter()
        .next()
        .expect("the address row is drawn");
    let output = click_at_output(&ctx, &mut app, copy.center());
    assert!(
        output.platform_output.commands.iter().any(|command| {
            matches!(
                command,
                egui::OutputCommand::CopyText(text) if text == "https://example.com/a"
            )
        }),
        "the href is what reaches the clipboard: {:?}",
        output.platform_output.commands
    );
}

/// A right-click on an image in the body offers its address: the loaded
/// texture is the target, not the mail's text around it.
#[test]
fn a_body_image_offers_its_address_on_right_click() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    let url = "https://example.com/pic.png";
    {
        let state = app.states.get_mut("acct").expect("the fixture account");
        let mail = &mut state.mailbox.folders[0].messages[0];
        mail.body_text = "A picture".to_string();
        mail.body_html = Some(format!("<p>A picture<img src=\"{url}\" alt=\"pic\"></p>"));
        state.folders_changed();
    }
    let pixels = [255u8, 0, 0, 255].repeat(4);
    let texture = ctx.load_texture(
        "body-image",
        egui::ColorImage::from_rgba_unmultiplied([2, 2], &pixels),
        Default::default(),
    );
    app.images
        .insert(url.to_string(), ImageStatus::Ready(texture));
    app.selected_message = Some("0".to_string());
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let window = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
    let pane = egui::Rect::from_min_max(
        egui::pos2(app.list_column.right(), TITLEBAR_HEIGHT),
        egui::pos2(1400.0, 900.0),
    );
    let image = image_rects_of(&shapes, pane)
        .into_iter()
        .next()
        .expect("the body draws its image");

    right_click_at(&ctx, &mut app, image.center());
    assert!(app.body_menu.is_some(), "the right-click reached the body");
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    for item in ["Open image", "Copy image address"] {
        assert!(
            !text_boxes_of(&shapes, window, item).is_empty(),
            "the body menu draws {item}"
        );
    }
    let copy = text_boxes_of(&shapes, window, "Copy image address")
        .into_iter()
        .next()
        .expect("the address row is drawn");
    let output = click_at_output(&ctx, &mut app, copy.center());
    assert!(
        output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text == url)
        }),
        "the image address reaches the clipboard: {:?}",
        output.platform_output.commands
    );
}

/// Clicking an attachment card starts a download: the app finds the account
/// and the mail the pane is showing, and hands the worker the part's place
/// and the file's name. What the worker does with them is `sync`'s own
/// test — this one is the click reaching it, which the account's spinner
/// (`Event::SyncStarted`, the first thing the job sends) is the sign of.
#[test]
fn clicking_an_attachment_card_starts_a_download() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.states
        .get_mut("acct")
        .expect("the fixture account")
        .mailbox
        .folders[0]
        .messages[0]
        .attachments = vec![Attachment {
        name: "report.pdf".to_string(),
        content_type: "application/pdf".to_string(),
        size: 68 * 1024,
    }];
    // Where the account is does not matter to the click, and the worker's
    // own failure is its own: a port with nothing on it keeps the job from
    // reaching off this machine.
    app.accounts[0].host = "127.0.0.1".to_string();
    app.accounts[0].port = 1;
    app.selected_account = Some("acct".to_string());
    app.selected_folder = Some("INBOX".to_string());
    app.selected_message = Some("0".to_string());

    let _ = frame_texts(&ctx, &mut app);
    let card = frame_rects(&ctx, &mut app)
        .into_iter()
        .find(|rect| (rect.height() - ATTACHMENT_CARD_HEIGHT).abs() < 0.5)
        .expect("the attachment card is on screen");
    let centre = card.center();
    run_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(centre)]);
    for pressed in [true, false] {
        run_app_frame(
            &ctx,
            &mut app,
            vec![egui::Event::PointerButton {
                pos: centre,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            }],
        );
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut started = false;
    while Instant::now() < deadline && !started {
        match app.rx.try_recv() {
            Ok(Event::SyncStarted { account_id }) => {
                assert_eq!(account_id, "acct");
                started = true;
            }
            Ok(_) => {}
            Err(mpsc::TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(5)),
            Err(mpsc::TryRecvError::Disconnected) => break,
        }
    }
    assert!(started, "a click on the card did not start a download");
}

/// A mail that came with parts says so in the list: a paperclip at the end
/// of its title line, drawn the same way for a folder row and for a search
/// hit. A mail with none draws none — and the search row says it with the
/// clip rather than with a second, textual paperclip in its third line.
#[test]
fn a_mail_with_attachments_carries_a_clip_in_the_list() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);

    let with_parts = |app: &mut KimoApp, count: usize| {
        let state = app.states.get_mut("acct").expect("the fixture account");
        state.mailbox.folders[0].messages[0].attachments = (0..count)
            .map(|index| Attachment {
                name: format!("file-{index}.pdf"),
                content_type: "application/pdf".to_string(),
                size: 1024,
            })
            .collect();
        // The list derives its rows from a revision, so a test that changes
        // a mail behind the app's back has to say so — the way a fetched
        // batch does (`AccountUiState::folders_changed`).
        state.folders_changed();
    };

    // A frame first: the list records its own rect as it draws.
    let _ = frame_texts(&ctx, &mut app);
    let mut column = app.list_column;
    let strokes = list_glyph_strokes(&frame_shapes_at(&ctx, &mut app, 1400.0), column);
    assert!(strokes.is_empty(), "an empty mail drew a clip: {strokes:?}");

    // One file: one clip, at the end of the title line — right of the
    // subject and inside the row's own text column.
    with_parts(&mut app, 1);
    column = app.list_column;
    let strokes = list_glyph_strokes(&frame_shapes_at(&ctx, &mut app, 1400.0), column);
    let clip = strokes
        .iter()
        .fold(None::<egui::Rect>, |bounds, rect| {
            Some(bounds.map_or(*rect, |b| b.union(*rect)))
        })
        .expect("the row of a mail with a file draws a clip");
    assert!(
        clip.width() > ROW_CLIP_WIDTH - 2.0
            && clip.width() <= ROW_CLIP_SIZE
            && clip.height() > ROW_CLIP_SIZE - 6.0
            && clip.height() <= ROW_CLIP_SIZE,
        "the clip fills the box the row reserves for it: {clip:?}"
    );
    assert!(
        clip.right() > column.right() - 30.0 && clip.right() <= column.right(),
        "the clip sits at the end of the title line: {clip:?} in {column:?}"
    );
    assert!(
        clip.center().y > column.top() + TITLEBAR_HEIGHT
            && clip.center().y < column.top() + TITLEBAR_HEIGHT + MESSAGE_ROW_HEIGHT,
        "and on the row itself rather than above or below it: {clip:?}"
    );

    // The search row draws the same clip and no longer spells it out.
    app.search.text = "report".to_string();
    app.search_hits = vec![SearchHit {
        account_id: "acct".to_string(),
        folder: "INBOX".to_string(),
        uid: "0".to_string(),
        subject: "Subject 0".to_string(),
        from: "Ada <ada@example.com>".to_string(),
        to: "me@example.com".to_string(),
        date: "2026-01-01".to_string(),
        timestamp: Some(1_700_000_000),
        seen: false,
        flagged: false,
        attachments: 1,
        snippet: "a line with report in it".to_string(),
    }];
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        !texts.iter().any(|text| text.contains('📎')),
        "the search row still spells the paperclip out: {texts:?}"
    );
    let column = app.list_column;
    let strokes = list_glyph_strokes(&frame_shapes_at(&ctx, &mut app, 1400.0), column);
    assert!(
        !strokes.is_empty(),
        "a search hit with a file draws no clip: {strokes:?}"
    );
}

/// The list's card is as tall as what the folder holds, not as tall as the
/// window: three mails leave the chrome showing below them, a folder that
/// outgrows the window fills the column as it always did, and a folder with
/// nothing in it has no card at all.
#[test]
fn the_lists_card_is_as_tall_as_the_mail_it_holds() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let content = theme::of(&ctx).content;

    let mut app = app_with_messages(&ctx, 3);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let column = app.list_column;
    let card = list_card_in(&shapes, column, content).expect("the list sits on a card");
    // The bar's height the first mail starts below, the rows, and nothing
    // below the last of them: the card ends with the mail.
    let expected = TITLEBAR_HEIGHT + 3.0 * MESSAGE_ROW_HEIGHT;
    assert!(
        (card.height() - expected).abs() < 1.0,
        "the card is {} tall, expected {expected}: {card:?}",
        card.height()
    );
    assert!(
        card.bottom() < column.bottom() - 1.0,
        "the chrome shows below the last mail: {card:?}"
    );

    // A folder that outgrows the window: the card fills the column.
    let mut app = app_with_messages(&ctx, 40);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let column = app.list_column;
    let card = list_card_in(&shapes, column, content).expect("the list sits on a card");
    assert!(
        (card.bottom() - column.bottom()).abs() < 1.0,
        "a list taller than the column fills it: {card:?} in {column:?}"
    );

    // Nothing to list: no card, just the chrome the placeholder stands on.
    let mut app = app_with_messages(&ctx, 0);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let column = app.list_column;
    assert!(
        list_card_in(&shapes, column, content).is_none(),
        "an empty folder leaves the chrome alone"
    );
}
/// A conversation is one card per mail on the pane, the same card a mail on
/// its own gets: the pane's own surface, as wide as the pane, with the
/// chrome showing between two of them. Nothing is nested — no card drawn
/// around the conversation with a card per mail inside it.
#[test]
fn a_conversation_draws_one_card_per_mail_on_the_pane() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_one_conversation(&ctx);
    app.selected_message = Some("3".to_string());
    // A frame to settle the columns: the list's own edge is what tells the
    // pane's cards apart from the surface the list is drawn on.
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
    let cards_of = |app: &mut KimoApp| pane_cards(&ctx, app);

    let cards = cards_of(&mut app);
    assert_eq!(cards.len(), 2, "one card per mail: {cards:?}");
    let (first, second) = (cards[0], cards[1]);
    let pane_left = app.list_column.right();
    for card in &cards {
        assert!(
            card.left() <= pane_left + CONTENT_GUTTER + 0.5
                && card.right() >= 1400.0 - CONTENT_GUTTER - 0.5,
            "a card spans the pane, gutter to gutter: {card:?}"
        );
    }
    // The first card starts below the floating title bar — the gutter under
    // the bar, like its other sides — so its top edge, and with it the two
    // rounded corners, is on screen rather than behind the glass.
    assert!(
        (first.top() - (TITLEBAR_HEIGHT + CONTENT_GUTTER)).abs() < 0.5,
        "the pane's first card starts under the bar's height: {first:?}"
    );
    let gap = second.top() - first.bottom();
    assert!(
        (gap - CONVERSATION_CARD_GAP).abs() < 0.5,
        "the chrome between two cards is {gap} pt tall"
    );
    assert!(
        !(first.contains_rect(second) || second.contains_rect(first)),
        "the cards are stacked, not one inside another: {cards:?}"
    );
    // The old shape: a card around all of them, and a card per mail inside
    // it — the faint fill and hairline an inset card is drawn with. The
    // pane has neither now; the list's own faint fills are its business.
    let inset = fills_in_color(
        &frame_shapes_at(&ctx, &mut app, 1400.0),
        theme::of(&ctx).card,
    );
    assert!(
        inset.iter().all(|rect| rect.center().x < pane_left),
        "no inset card left in the pane: {inset:?}"
    );

    // And the card a mail on its own gets, with threading off, is the same
    // card: same edges, same width.
    app.threaded = false;
    let alone = cards_of(&mut app);
    assert_eq!(alone.len(), 1, "a mail of its own is one card: {alone:?}");
    assert_eq!(alone[0], first, "a conversation's card is the pane's card");
}

/// What a card's second line is made of, and the sizes it prints: the type
/// and the size each appear when the mail was stored with them and are
/// simply absent when it was not.
#[test]
fn a_card_says_what_the_mail_was_stored_with() {
    let full = Attachment {
        name: "report.pdf".to_string(),
        content_type: "application/pdf".to_string(),
        size: 68 * 1024,
    };
    assert_eq!(attachment_meta(&full), "application/pdf · 68 KB");
    assert_eq!(
        attachment_meta(&Attachment {
            content_type: String::new(),
            ..full.clone()
        }),
        "68 KB",
        "a size with no type still says something"
    );
    assert_eq!(
        attachment_meta(&Attachment {
            size: 0,
            ..full.clone()
        }),
        "application/pdf"
    );
    assert_eq!(
        attachment_meta(&Attachment {
            name: "old.pdf".to_string(),
            content_type: String::new(),
            size: 0,
        }),
        "",
        "a row from before v6 has nothing to say about itself"
    );

    assert_eq!(human_size(0), "0 B");
    assert_eq!(human_size(1023), "1023 B");
    assert_eq!(human_size(1024), "1 KB", "a round size loses its .0");
    assert_eq!(human_size(1536), "1.5 KB");
    assert_eq!(human_size(5 * 1024 * 1024), "5 MB");
    assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3 GB");
    assert_eq!(human_size(u64::MAX), "16777216 TB", "and it stops at TB");
}

/// The scope the body is laid out in is what holds its width: the card has
/// already drawn its headers across the pane, and egui refuses to make a ui
/// narrower than what it has drawn, so this is the check that the mail is
/// really wrapped for the width the cache holds and not for the pane's.
#[test]
fn the_body_is_wrapped_for_the_width_the_cache_holds() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    let html = format!(
        "<p>{}</p>",
        "the quick brown fox jumps over the lazy dog ".repeat(40)
    );
    app.states
        .get_mut("acct")
        .unwrap()
        .mailbox
        .folders
        .first_mut()
        .unwrap()
        .messages
        .first_mut()
        .unwrap()
        .body_html = Some(html.clone());
    let msg = app.folder_messages_of("acct", "INBOX").unwrap()[0].clone();

    let images: HashMap<String, egui::TextureHandle> = HashMap::new();
    let inline: HashMap<String, egui::TextureHandle> = HashMap::new();
    let sheets: HashMap<String, String> = HashMap::new();
    // The height of a body laid out inside a `pane`-wide ui, through a
    // scope of `laid_out` width — the nesting `mail_body` makes.
    let height = |app: &mut KimoApp, pane: f32, laid_out: f32, body: Option<&MailMessage>| {
        let mut measured = 0.0_f32;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 4000.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            let pane_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(pane, 4000.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(pane_rect), |ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                match body {
                    Some(msg) => app.mail_body(ui, msg),
                    None => {
                        let body_rect = egui::Rect::from_min_size(
                            ui.cursor().min,
                            egui::vec2(laid_out, ui.max_rect().bottom()),
                        );
                        let document = crate::html::Document::parse(&html);
                        let mut html_ctx = crate::html::HtmlContext {
                            images: &images,
                            inline_images: &inline,
                            stylesheets: &sheets,
                            missing_images: Vec::new(),
                            missing_stylesheets: Vec::new(),
                            context: None,
                        };
                        ui.scope_builder(egui::UiBuilder::new().max_rect(body_rect), |ui| {
                            crate::html::render_document(ui, &document, &mut html_ctx);
                        });
                    }
                }
                measured = ui.min_rect().height();
            });
        });
        output.textures_delta.clear();
        measured
    };

    // The same body at two widths: the narrower pane wraps it taller, so
    // the two heights tell the two layouts apart.
    let wide = height(&mut app, 500.0, 500.0, None);
    let narrow = height(&mut app, 300.0, 300.0, None);
    assert!(wide < narrow - 20.0, "{wide} vs {narrow}");

    // The body has settled at 500 pt…
    app.body.get(
        &ctx,
        "INBOX/0",
        body_fingerprint(&html),
        &html,
        500.0,
        Instant::now(),
    );
    // …and the pane is 300 pt wide now. The mail keeps the layout it has:
    // the height is the wide one, not the height the pane would wrap it to.
    let held = height(&mut app, 300.0, 300.0, Some(&msg));
    assert!(
        (held - wide).abs() < 2.0,
        "laid out at {held}, expected the held width's {wide} (narrow would be {narrow})"
    );
}

#[test]
fn the_body_keeps_its_width_while_the_pane_moves() {
    let ctx = egui::Context::default();
    let start = Instant::now();
    let mut cache = BodyCache::default();
    let html = "<p>Hello</p>";
    let fingerprint = body_fingerprint(html);
    let width_at = |cache: &mut BodyCache, wanted, now| {
        cache.get(&ctx, "INBOX/1", fingerprint, html, wanted, now).1
    };

    // A body the cache has not seen is laid out at the pane's own width.
    assert_eq!(width_at(&mut cache, 500.0, start), 500.0);
    // A pane that moved is not a reflow: the body keeps the width it has,
    // and the clock starts.
    assert_eq!(width_at(&mut cache, 480.0, start), 500.0);
    // Still moving: same width, clock restarted.
    assert_eq!(
        width_at(&mut cache, 460.0, start + Duration::from_millis(100)),
        500.0
    );
    // Held still for the settle time: the body reflows, once.
    assert_eq!(
        width_at(
            &mut cache,
            460.0,
            start + Duration::from_millis(100) + GEOMETRY_SETTLE
        ),
        460.0
    );
    // The width it has now is not a reflow again.
    assert_eq!(
        width_at(
            &mut cache,
            460.0,
            start + Duration::from_millis(100) + GEOMETRY_SETTLE * 2
        ),
        460.0
    );
    // And a width the pane only visits for a moment never reaches the body.
    assert_eq!(
        width_at(
            &mut cache,
            700.0,
            start + Duration::from_millis(100) + GEOMETRY_SETTLE * 3
        ),
        460.0
    );
}

#[test]
fn the_body_cache_reparses_a_body_that_changed() {
    let ctx = egui::Context::default();
    let now = Instant::now();
    let mut cache = BodyCache::default();
    let first = "<p>one</p>";
    let second = "<p>two</p>";
    let laid_out_at = |cache: &mut BodyCache, identity: &str, html: &str, wanted| {
        cache
            .get(&ctx, identity, body_fingerprint(html), html, wanted, now)
            .1
    };

    // First sight of a body: laid out at the pane's own width.
    assert_eq!(laid_out_at(&mut cache, "INBOX/1", first, 400.0), 400.0);
    // The same body a frame later, with the pane moved: the width it has.
    assert_eq!(laid_out_at(&mut cache, "INBOX/1", first, 380.0), 400.0);
    // Another mail is a new body, and a new body is laid out at once
    // rather than held at the width the old one was laid out for. The
    // first mail keeps its own entry: a conversation shows several at once.
    assert_eq!(cache.entries.len(), 1);
    assert_eq!(laid_out_at(&mut cache, "INBOX/2", first, 320.0), 320.0);
    assert_eq!(cache.entries.len(), 2);
    // The first one is still there, at the width it settled at.
    assert_eq!(laid_out_at(&mut cache, "INBOX/1", first, 300.0), 400.0);
    // Another body under the same uid is a new body: a re-fetch can replace
    // one the pane is already showing.
    assert_eq!(laid_out_at(&mut cache, "INBOX/2", second, 300.0), 300.0);
    assert_eq!(
        cache.entries[1].fingerprint,
        body_fingerprint(second),
        "the entry was parsed again, not left as it was"
    );
    // Switching renderer, or opening a mail with no HTML at all, drops it.
    cache.forget();
    assert!(cache.entries.is_empty());
    assert_eq!(laid_out_at(&mut cache, "INBOX/2", second, 280.0), 280.0);
}

/// A conversation is several bodies at once, and the cache is bounded: the
/// mail that has been off screen longest is the one a new body evicts.
#[test]
fn the_body_cache_holds_a_conversation_and_evicts_the_oldest() {
    let ctx = egui::Context::default();
    let start = Instant::now();
    let mut cache = BodyCache::default();
    let html = "<p>Hello</p>";
    let fingerprint = body_fingerprint(html);
    let at = |cache: &mut BodyCache, uid: &str, now| {
        cache.get(&ctx, uid, fingerprint, html, 400.0, now).1
    };

    // Every mail of a conversation is held, not just the last one asked
    // for: the pane walks all of them on every frame.
    for index in 0..BODY_CACHE {
        at(&mut cache, &format!("INBOX/{index}"), start);
    }
    assert_eq!(cache.entries.len(), BODY_CACHE);
    // One more evicts the mail used longest ago — the first, which nothing
    // has asked for since.
    at(&mut cache, "INBOX/extra", start + Duration::from_millis(10));
    assert_eq!(cache.entries.len(), BODY_CACHE);
    assert!(
        !cache
            .entries
            .iter()
            .any(|entry| entry.identity == "INBOX/0"),
        "the oldest entry is the one that went"
    );
    assert!(cache
        .entries
        .iter()
        .any(|entry| entry.identity == "INBOX/extra"));
}

#[test]
fn blitz_pages_are_reused_only_for_a_slightly_narrower_pane() {
    // Within the reuse window: scale the existing raster down.
    assert!(blitz_page_reusable(864.0, 864.0));
    assert!(blitz_page_reusable(864.0, 840.0));
    assert!(blitz_page_reusable(864.0, 700.0));
    // Much narrower: re-flowing the text is worth a render.
    assert!(!blitz_page_reusable(864.0, 600.0));
    // Wider than the raster: a page is never scaled up.
    assert!(!blitz_page_reusable(864.0, 900.0));
}

/// A Blitz page is drawn on a paper, and the paper is *under* the mail: white
/// by default, so a mail written for white paper is readable in either theme
/// — while a mail that paints a background of its own has it painted across
/// the whole page by the renderer, over this.
#[test]
fn a_blitz_page_is_drawn_on_white_paper_under_the_mail() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    html_body(&mut app, 0);

    let page = egui::vec2(400.0, 200.0);
    let (shapes, tile) = blitz_frame_with_page(&ctx, &mut app, "0", page);
    assert!(
        paper_fill(&shapes, page, egui::Color32::WHITE),
        "the page is drawn on white paper"
    );

    let ordered = shapes_in_order(&shapes);
    let paper = ordered.iter().position(|shape| {
        matches!(shape, egui::Shape::Rect(rect)
                if rect.fill == egui::Color32::WHITE && (rect.rect.size() - page).length() < 0.5)
    });
    let mail = ordered
        .iter()
        .position(|shape| matches!(shape, egui::Shape::Mesh(mesh) if mesh.texture_id == tile));
    assert!(
        matches!((paper, mail), (Some(paper), Some(mail)) if paper < mail),
        "the paper goes under the page: paper {paper:?}, page {mail:?}"
    );
}

/// The cell is the Blitz page's own: Simple mode draws the mail as widgets on
/// the card and has no paper to switch, and a plain-text mail has no page at
/// all — so neither draws the cell.
#[test]
fn the_paper_cell_is_only_drawn_for_a_blitz_page() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 2);
    html_body(&mut app, 0);
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());

    let cell = header_action_center(5);
    assert_eq!(app.html_renderer, HtmlRenderer::Simple);
    assert!(
        fill_under_the_pointer(&ctx, &mut app, 1400.0, cell, CARD_ACTION_SIZE).is_none(),
        "Simple mode draws no paper cell"
    );

    app.html_renderer = HtmlRenderer::Blitz;
    app.selected_message = Some("1".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());
    assert!(
        fill_under_the_pointer(&ctx, &mut app, 1400.0, cell, CARD_ACTION_SIZE).is_none(),
        "a plain-text mail has no page to draw on"
    );

    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());
    assert!(
        fill_under_the_pointer(&ctx, &mut app, 1400.0, cell, CARD_ACTION_SIZE).is_some(),
        "the mail drawn by Blitz carries the cell"
    );
}

/// A mail's head is its sender: the list's own avatar at the card's
/// top-left, the sender's name beside it, and the mail's four actions at
/// that row's right end — all of it above the subject, which is what the
/// row is there for. The address line the card used to draw is gone with
/// it: the sender is the head, not a `From:` label under one.
#[test]
fn a_mails_card_puts_its_sender_first_with_the_actions_on_that_row() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    app.selected_message = Some("0".to_string());
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);

    // The avatar is the list's own circle, in the card's own padding —
    // the row the action cells are centred in.
    let pane = egui::Rect::from_min_max(
        egui::pos2(app.list_column.right(), 0.0),
        egui::pos2(1400.0, 900.0),
    );
    let avatars = avatars_in(&shapes, pane);
    assert_eq!(avatars.len(), 1, "the head draws one avatar: {avatars:?}");
    let (centre, diameter) = avatars[0];
    assert!((diameter - AVATAR_DIAMETER).abs() < 0.5);
    let card_top = TITLEBAR_HEIGHT + CONTENT_GUTTER;
    assert!(
        (centre.x - (pane.left() + CONTENT_GUTTER + CONTENT_PADDING.x + AVATAR_DIAMETER / 2.0))
            .abs()
            < 1.0
            && (centre.y - (card_top + CONTENT_PADDING.y + AVATAR_DIAMETER / 2.0)).abs() < 1.0,
        "the avatar starts the head row: {centre:?}"
    );

    // The sender is on the card; the `From:` line is not.
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts.iter().any(|text| text == "Ada <ada@example.com>"),
        "the sender's own line is on the card: {texts:?}"
    );
    assert!(
        !texts.iter().any(|text| text.starts_with("From: ")),
        "the head is the sender, not a From: line: {texts:?}"
    );

    // The actions are cells of the card's own, smaller than the bar's:
    // what the pointer fills under one is `CARD_ACTION_SIZE` square,
    // where a title-bar cell fills the whole `TITLEBAR_BUTTON_SIZE`.
    let scheme = theme::of(&ctx);
    let cell = header_action_center(4);
    let filled = fill_under_the_pointer(&ctx, &mut app, 1400.0, cell, CARD_ACTION_SIZE)
        .unwrap_or_else(|| panic!("nothing filled under the reply cell at {cell:?}"));
    assert_eq!(filled.rect.center(), cell);
    assert_eq!(
        filled.corner_radius,
        egui::CornerRadius::same(TITLEBAR_BUTTON_CORNER_RADIUS),
        "the card's cells fill the way the bar's do"
    );

    // And the glyph is weak at rest, the text color under the pointer:
    // the four are on the mail's own head, so none of them shouts until
    // the pointer says which one is being read.
    let reply = egui::Rect::from_center_size(cell, egui::Vec2::splat(CARD_ACTION_SIZE));
    let rest = glyph_colors_in(&shapes, reply);
    assert!(
        rest.contains(&scheme.text_weak) && !rest.contains(&scheme.text),
        "the reply glyph is weak while the pointer is away: {rest:?}"
    );
    let hovered = glyph_colors_in(&frame_shapes_at(&ctx, &mut app, 1400.0), reply);
    assert!(
        hovered.contains(&scheme.text) && !hovered.contains(&scheme.text_weak),
        "the pointer brings the reply glyph to the text color: {hovered:?}"
    );
}
