use super::super::*;
use super::common::*;

/// The same order, live: a window crossing below what the three columns
/// need collapses the navigation, a narrower one caps the list beside the
/// reading pane's minimum, and widening again brings the list back to the
/// width its splitter left it at — which egui on its own would have
/// forgotten in favour of the last width it was capped at. The pane is
/// the column that takes the room the window gives back.
#[test]
fn the_list_is_capped_and_comes_back_at_its_own_width() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    let settle = |app: &mut KimoApp, width: f32| {
        for _ in 0..30 {
            run_app_frame_sized(&ctx, app, Vec::new(), egui::vec2(width, 900.0));
        }
    };

    // Wide: the three columns at their defaults, and the reading pane is
    // what the two side columns leave.
    settle(&mut app, 1400.0);
    assert!(!app.nav_collapsed);
    assert!(!app.list_capped);
    assert_eq!(app.list_width, LIST_DEFAULT_WIDTH);
    assert!((app.list_column.width() - LIST_DEFAULT_WIDTH).abs() < 0.5);
    assert!((1400.0 - app.list_column.right() - 740.0).abs() < 0.5);

    // One point below navigation + list + the pane's minimum: the
    // navigation gives way, and the list keeps its own width while the
    // pane takes what the navigation left.
    settle(&mut app, 939.0);
    assert!(app.nav_collapsed);
    assert!(!app.list_capped);
    assert!((app.list_column.width() - LIST_DEFAULT_WIDTH).abs() < 0.5);
    assert!((939.0 - app.list_column.right() - 479.0).abs() < 0.5);

    // Narrower still: the list is capped at what leaves the pane its
    // minimum, and the width to bring it back to is the app's own.
    settle(&mut app, 600.0);
    assert!(app.list_capped);
    assert!((app.list_column.width() - 320.0).abs() < 0.5);
    assert!((600.0 - app.list_column.right() - CONTENT_MIN_WIDTH).abs() < 0.5);
    assert_eq!(app.list_width, LIST_DEFAULT_WIDTH);

    // Widening lifts the cap and restores the list's own width. The
    // navigation stays collapsed — re-expanding is a manual choice, the
    // way it always was — so the pane has everything but the list.
    settle(&mut app, 1400.0);
    assert!(!app.list_capped);
    assert!((app.list_column.width() - LIST_DEFAULT_WIDTH).abs() < 0.5);
    assert!((1400.0 - app.list_column.right() - 940.0).abs() < 0.5);
    assert_eq!(app.list_width, LIST_DEFAULT_WIDTH);
}

/// The other way round, for the file the app writes without being asked
/// where: a downloaded attachment's line above the message list — the line
/// the export used to get — and the timing that takes it away again. The
/// line is one of two things the app says out loud on its own (`sent_notice`
/// is the other), and a line that stayed would be furniture.
#[test]
fn the_line_above_the_list_says_where_a_download_went_and_is_timed() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.apply_event(Event::AttachmentDownloaded {
        path: std::path::PathBuf::from("/home/ada/Downloads/report.pdf"),
    });

    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts
            .iter()
            .any(|text| text == "Saved to /home/ada/Downloads/report.pdf"),
        "{texts:?}"
    );

    // Past its time the line is gone, and the app has stopped asking to be
    // repainted for it.
    let stale = Instant::now()
        .checked_sub(NOTICE_TIME + Duration::from_millis(1))
        .expect("the machine has been up longer than the notice lasts");
    app.notice = app.notice.take().map(|(text, _)| (text, stale));
    let texts = frame_texts(&ctx, &mut app);
    assert!(app.notice.is_none(), "the notice outlived its time");
    assert!(
        !texts.iter().any(|text| text.starts_with("Saved to")),
        "{texts:?}"
    );
}

/// The row's three lines are one block, centred in the row: the room above
/// the sender's line is the room below the preview. Placing them from the
/// row's top edge instead left the preview half as far from the bottom as
/// the sender was from the top, which is what the eye reads as a row whose
/// mail is falling out of it.
#[test]
fn the_rows_three_lines_are_centred_in_the_row() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 2);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let column = app.list_column;
    let row = egui::Rect::from_min_size(
        egui::pos2(column.left(), column.top() + TITLEBAR_HEIGHT),
        egui::vec2(column.width(), MESSAGE_ROW_HEIGHT),
    );

    // Everything the row draws text with, the avatar's initial included:
    // the block the eye sees is all of it.
    let lines = text_boxes_in(&shapes, row);
    assert!(
        lines.len() >= 4,
        "the row's sender, date, subject and preview: {lines:?}"
    );
    let top = lines
        .iter()
        .fold(f32::INFINITY, |top, line| top.min(line.top()));
    let bottom = lines
        .iter()
        .fold(f32::NEG_INFINITY, |bottom, line| bottom.max(line.bottom()));
    let middle = (top + bottom) / 2.0;
    assert!(
        (middle - row.center().y).abs() < 2.5,
        "the row's text is centred on {}, not {middle}: {lines:?}",
        row.center().y
    );
}

/// A mail of an open conversation is drawn as one of the conversation's
/// own: a step further in than the rows around it, with a smaller avatar,
/// and no guide line down the chevron's column — the step and the smaller
/// circle are what say, at a glance, that it belongs to the conversation
/// above it.
#[test]
fn the_mails_of_an_open_conversation_step_in_with_a_smaller_avatar() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_one_conversation(&ctx);
    // A frame to derive the rows, then the conversation is opened the way
    // the chevron opens it.
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
    let key = app
        .list_rows
        .as_ref()
        .expect("the list derived its rows")
        .threads[0]
        .key
        .clone();
    app.open_threads.insert(key);
    app.open_rev += 1;

    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let column = app.list_column;
    let avatars = avatars_in(&shapes, column);
    assert_eq!(
        avatars.len(),
        3,
        "the conversation's row and the two mails under it: {avatars:?}"
    );
    let (row, mails) = (avatars[0], &avatars[1..]);
    assert_eq!(
        row.1, AVATAR_DIAMETER,
        "the conversation's own row keeps the full avatar"
    );
    for (centre, diameter) in mails {
        assert_eq!(
            *diameter, THREAD_AVATAR_DIAMETER,
            "a mail of the conversation draws the smaller avatar: {avatars:?}"
        );
        assert!(
            centre.x > row.0.x + 8.0,
            "the mail steps in past the conversation's row: {avatars:?}"
        );
    }

    // Nothing is drawn down the chevron's column: what marks the mails as
    // the conversation's is the step and the smaller avatar, not a line.
    let lines = vertical_lines_in(&shapes, column);
    assert!(
        lines.is_empty(),
        "a mail of an open conversation still draws a line down the column: {lines:?}"
    );
}

/// The rows a folder becomes, without a frame in the way: one per
/// conversation, its mails under it once it is open, and one per mail when
/// the list is not threading.
#[test]
fn the_rows_are_one_per_conversation_and_its_mails_once_open() {
    let mails = vec![
        threaded_mail("3", "Re: Hello", "c@x", &["a@x", "b@x"], 300),
        threaded_mail("2", "Re: Hello", "b@x", &["a@x"], 200),
        threaded_mail("1", "Hello", "a@x", &[], 100),
        threaded_mail("4", "Groceries", "d@x", &[], 50),
    ];
    let threads = threads::group(&mails);
    assert_eq!(threads.len(), 2);

    let closed: std::collections::HashSet<String> = std::collections::HashSet::new();
    assert_eq!(
        list_rows_of(&threads, &all_in_order(&mails), &closed, true),
        vec![
            ListRow::Thread { thread: 0 },
            // A conversation of one is a mail, and is drawn like one: no
            // chevron, no indent, nothing to open.
            ListRow::Message {
                index: 3,
                in_thread: false
            },
        ]
    );

    // Open: the conversation's own row, then its mails oldest first.
    let open: std::collections::HashSet<String> = [threads[0].key.clone()].into_iter().collect();
    assert_eq!(
        list_rows_of(&threads, &all_in_order(&mails), &open, true),
        vec![
            ListRow::Thread { thread: 0 },
            ListRow::Message {
                index: 2,
                in_thread: true
            },
            ListRow::Message {
                index: 1,
                in_thread: true
            },
            ListRow::Message {
                index: 0,
                in_thread: true
            },
            ListRow::Message {
                index: 3,
                in_thread: false
            },
        ]
    );

    // Not threading: every mail is a row of its own.
    assert_eq!(
        list_rows_of(&threads, &all_in_order(&mails), &open, false),
        (0..mails.len())
            .map(|index| ListRow::Message {
                index,
                in_thread: false
            })
            .collect::<Vec<_>>()
    );
}

/// Clicking a conversation's row opens it in the reading pane — every mail
/// it holds, oldest first, as cards of their own — and leaves the list
/// exactly as it was: opening the list's own rows is the chevron's business.
#[test]
fn clicking_a_conversation_opens_it_in_the_pane_and_not_in_the_list() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_one_conversation(&ctx);
    let row = a_conversation_row(&mut app, &ctx);
    let before = app.list_rows.as_ref().expect("rows").rows.clone();

    click_at(&ctx, &mut app, row);

    assert_eq!(
        app.selected_message.as_deref(),
        Some("3"),
        "the conversation opens on the mail its row speaks for"
    );
    let rows = app.list_rows.as_ref().expect("rows");
    assert_eq!(rows.rows, before, "the list did not change shape");

    // The pane holds the whole conversation: both mails, in reading order.
    let texts = frame_texts(&ctx, &mut app);
    let oldest = texts
        .iter()
        .position(|text| text == "body of 1")
        .unwrap_or_else(|| panic!("the oldest mail is not on the pane: {texts:?}"));
    let newest = texts
        .iter()
        .position(|text| text == "body of 3")
        .unwrap_or_else(|| panic!("the newest mail is not on the pane: {texts:?}"));
    assert!(
        oldest < newest,
        "a conversation reads oldest first: {texts:?}"
    );
}

/// The chevron is what opens and closes a conversation's mails in the list,
/// and it does nothing else: no mail is opened by it, and the pane is left
/// where it was.
#[test]
fn the_chevron_opens_and_closes_the_mails_under_a_conversation() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_one_conversation(&ctx);
    let row = a_conversation_row(&mut app, &ctx);
    let chevron = egui::pos2(app.list_column.left() + 12.0, row.y);

    click_at(&ctx, &mut app, chevron);
    // The click is applied at the end of the frame that saw it, so the rows
    // it opened are the next frame's to derive.
    let _ = frame_texts(&ctx, &mut app);
    let rows = app.list_rows.as_ref().expect("rows").rows.clone();
    assert!(
        rows.iter().any(|row| matches!(
            row,
            ListRow::Message {
                in_thread: true,
                ..
            }
        )),
        "the mails are under the row now: {rows:?}"
    );
    assert_eq!(
        app.selected_message, None,
        "opening a conversation in the list does not open a mail"
    );

    // The chevron of the first row has not moved, so the same click closes
    // it again.
    click_at(&ctx, &mut app, chevron);
    let _ = frame_texts(&ctx, &mut app);
    let rows = app.list_rows.as_ref().expect("rows").rows.clone();
    assert!(
        rows.iter().all(|row| matches!(row, ListRow::Thread { .. })),
        "the conversation closed again: {rows:?}"
    );
}

/// Threading is a preference, and it is the list's shape: turned off, every
/// mail is a row of its own and nothing is grouped.
#[test]
fn the_list_threads_only_when_it_is_asked_to() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mails = vec![
        threaded_mail("2", "Re: Hello", "b@example.com", &["a@example.com"], 200),
        threaded_mail("1", "Hello", "a@example.com", &[], 100),
    ];
    let mut app = app_with_threaded_mail(&ctx, mails.clone());
    app.threaded = false;

    let texts = frame_texts(&ctx, &mut app);
    for subject in ["Hello", "Re: Hello"] {
        assert!(
            texts.iter().any(|text| text == subject),
            "every mail is a row of its own: {texts:?}"
        );
    }
    let rows = app.list_rows.as_ref().expect("the list derived its rows");
    assert_eq!(rows.rows.len(), mails.len());
    assert!(
        rows.rows.iter().all(|row| matches!(
            row,
            ListRow::Message {
                in_thread: false,
                ..
            }
        )),
        "a flat list has no conversation rows: {:?}",
        rows.rows
    );
}

/// Clicking a row opens that mail: the list draws itself from the account
/// state and only remembers what was clicked, so this is the test that the
/// click still lands on the mail under the pointer (`folder_messages`).
#[test]
fn clicking_a_row_opens_that_mail() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 5);
    run_app_frame(&ctx, &mut app, Vec::new());

    // The second row: below the title bar's padding, one row down, inside
    // the message column (the navigation is 200 pt wide).
    let pos = egui::pos2(300.0, TITLEBAR_HEIGHT + MESSAGE_ROW_HEIGHT * 1.5);
    run_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(pos)]);
    for pressed in [true, false] {
        run_app_frame(
            &ctx,
            &mut app,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            }],
        );
    }

    assert_eq!(app.selected_message.as_deref(), Some("1"));
    // Opening it marks it seen on the spot — the row's own copy, not just
    // the database's.
    assert!(app.folder_messages_of("acct", "INBOX").unwrap()[1].seen);
}

/// Scroll the list, then click a row: the row the pointer lands on is the
/// mail that opens, which is the whole point of laying out only the rows
/// the viewport shows (`virtual_rows`) and of the ids that keep them
/// stable as the list scrolls.
#[test]
fn clicking_a_row_after_scrolling_opens_the_mail_that_is_there() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 60);
    run_app_frame(&ctx, &mut app, Vec::new());

    let over_list = egui::pos2(300.0, 400.0);
    run_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(over_list)]);
    run_app_frame(
        &ctx,
        &mut app,
        vec![egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -400.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::default(),
        }],
    );
    // The scroll area animates its offset: let it come to rest.
    for _ in 0..30 {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    let offset = app.list_scroll;
    assert!(offset > 0.0, "the list did not scroll");

    // Something comfortably inside the viewport, in the list's padding
    // below the title bar; the row there is the one the offset puts there.
    let y = TITLEBAR_HEIGHT + MESSAGE_ROW_HEIGHT * 2.5;
    let expected = ((y - TITLEBAR_HEIGHT + offset) / MESSAGE_ROW_HEIGHT).floor() as usize;
    let pos = egui::pos2(300.0, y);
    run_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(pos)]);
    for pressed in [true, false] {
        run_app_frame(
            &ctx,
            &mut app,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            }],
        );
    }

    assert_eq!(
        app.selected_message.as_deref(),
        Some(expected.to_string().as_str())
    );
}

/// A conversation's row stands for every mail it holds, so its box takes
/// them all in — and out — at once. Checking one mail of it in the list
/// under it leaves the conversation's own box saying *some*, which is what
/// the dash is for.
#[test]
fn a_conversations_box_takes_its_whole_thread_in_and_out() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_one_conversation(&ctx);
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

    let at = checkbox_center(&app, 0, false);
    click_at(&ctx, &mut app, at);
    assert_eq!(app.selected_mails.len(), 2, "the whole conversation");
    for uid in ["3", "1"] {
        assert!(
            app.selected_mails
                .contains(&MailKey::new("acct", "INBOX", uid)),
            "mail {uid} is checked"
        );
    }

    // Open the conversation the way the chevron does: its mails get rows —
    // and boxes — of their own, oldest first under it.
    let key = app
        .list_rows
        .as_ref()
        .expect("the list derived its rows")
        .threads[0]
        .key
        .clone();
    app.open_threads.insert(key);
    app.open_rev += 1;
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
    let at = checkbox_center(&app, 1, true);
    click_at(&ctx, &mut app, at);
    assert_eq!(
        app.selected_mails.len(),
        1,
        "unchecking one mail of the conversation leaves the other"
    );
    assert_eq!(
        app.folder_checkbox(&["3", "1"]),
        Some(CheckState::Partial),
        "the conversation's own box says some of it is checked"
    );
}

/// A row offers its own delete button under the pointer: the timestamp gives
/// way to a muted trash glyph, and the glyph takes the scheme's danger fill
/// once the pointer is on the button itself. Nothing about the row's layout
/// moves — the sender is elided against the timestamp's slot either way.
#[test]
fn the_pointer_puts_a_delete_button_on_the_row_it_is_on() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    let date = {
        let msg = &app.folder_messages_of("acct", "INBOX").expect("mail")[1];
        date_label(msg.timestamp, &msg.date)
    };
    let danger = theme::of(&ctx).danger_hover;

    // The pointer away: every row's timestamp stands — the fixture's mails
    // share one, so the count is what says one of them went — and no button
    // is drawn.
    let texts = frame_texts(&ctx, &mut app);
    assert_eq!(
        texts.iter().filter(|text| *text == &date).count(),
        3,
        "the timestamps stand while the pointer is away: {texts:?}"
    );
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(
        fills_in_color(&shapes, danger).is_empty(),
        "nothing is filled at rest"
    );

    // The pointer on the row, away from the button: the timestamp gives way
    // and the glyph is drawn in its slot.
    let row = checkbox_center(&app, 1, false);
    let over_row = egui::pos2(320.0, row.y);
    frame_shapes_with(
        &ctx,
        &mut app,
        1400.0,
        vec![egui::Event::PointerMoved(over_row)],
    );
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let texts = frame_texts(&ctx, &mut app);
    assert_eq!(
        texts.iter().filter(|text| *text == &date).count(),
        2,
        "the hovered row's timestamp gave way to the button: {texts:?}"
    );
    let cell = egui::Rect::from_center_size(
        row_delete_center(&app, 1),
        egui::Vec2::splat(ROW_ACTION_SIZE),
    );
    assert_eq!(
        strokes_in(&shapes, cell).len(),
        3,
        "the trash can is three strokes on its slot"
    );
    assert!(
        fills_in_color(&shapes, danger).is_empty(),
        "and it is not filled until the pointer is on the button"
    );

    // The pointer on the button itself: the danger fill under the glyph.
    let at = row_delete_center(&app, 1);
    frame_shapes_with(&ctx, &mut app, 1400.0, vec![egui::Event::PointerMoved(at)]);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(
        fills_in_color(&shapes, danger)
            .iter()
            .any(|rect| rect.center() == at),
        "the button takes the danger fill under the pointer"
    );
}

/// The button is a target of its own: a click on it is not a click on the
/// row, so the mail it deletes is not the mail the reading pane opens.
#[test]
fn the_row_delete_button_is_not_a_click_on_the_mail() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

    let at = row_delete_center(&app, 1);
    click_at(&ctx, &mut app, at);
    assert_eq!(
        app.selected_message, None,
        "the click deleted rather than opened the mail"
    );
}

/// A right-click on a message row opens the mail actions for the mail the
/// row speaks for, at the pointer: the title bar's run plus Open, and the
/// flag's wording turning over with the row. A click on Flag is the title
/// bar's flag action, not a second one.
#[test]
fn a_message_row_offers_its_actions_on_right_click() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 2);
    app.accounts[0].host = "127.0.0.1".to_string();
    app.accounts[0].port = 1;
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let row = text_boxes_of(&shapes, app.list_column, "Subject 1")
        .into_iter()
        .next()
        .expect("the second row is drawn");

    right_click_at(&ctx, &mut app, row.center());
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let window = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
    for item in [
        "Open",
        "Reply",
        "Reply all",
        "Forward",
        "Flag",
        "Export",
        "Delete",
    ] {
        assert!(
            !text_boxes_of(&shapes, window, item).is_empty(),
            "the row's context menu draws {item}: {shapes:?}"
        );
    }

    let flag = text_boxes_of(&shapes, window, "Flag")
        .into_iter()
        .next()
        .expect("the Flag row is drawn");
    click_at(&ctx, &mut app, flag.center());
    assert!(
        app.folder_messages_of("acct", "INBOX").unwrap()[1].flagged,
        "the menu's Flag marks the mail the row speaks for"
    );
}

/// A conversation's row speaks for its newest mail — the one the reading pane
/// opens from it — and that is the mail its own button takes, not the whole
/// thread: the thread's mails are the box's (all at once) or the title bar's
/// (one at a time) to delete.
#[test]
fn a_conversations_delete_button_takes_the_mail_its_row_speaks_for() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_one_conversation(&ctx);
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

    let rows = app.list_rows.as_ref().expect("the list derived its rows");
    let messages = app.folder_messages_of("acct", "INBOX").expect("mail");
    let conversation = rows.rows[0];
    assert!(
        matches!(conversation, ListRow::Thread { .. }),
        "the fixture's row is a conversation: {:?}",
        conversation
    );
    assert_eq!(
        row_action_target(conversation, rows, messages).map(|msg| msg.uid.as_str()),
        Some("3"),
        "the newest mail of the thread, not the whole of it"
    );

    // A plain mail row takes the mail it is.
    let plain = ListRow::Message {
        index: 1,
        in_thread: false,
    };
    assert_eq!(
        row_action_target(plain, rows, messages).map(|msg| msg.uid.as_str()),
        Some(messages[1].uid.as_str())
    );
}

/// What the button says it takes: the same wording as the title bar's Delete
/// for a mail, and the newest mail named outright for a conversation, since
/// one click there takes one mail of it.
#[test]
fn the_row_delete_button_says_what_it_takes() {
    assert_eq!(
        row_delete_tooltip(RowPlace::Plain),
        MailAction::Delete.tooltip()
    );
    assert_eq!(
        row_delete_tooltip(RowPlace::InThread),
        MailAction::Delete.tooltip()
    );
    let thread = row_delete_tooltip(RowPlace::Thread {
        open: false,
        count: 3,
        key: "1",
    });
    assert!(
        thread.contains("newest mail"),
        "a conversation's button says which mail it takes: {thread}"
    );
}

/// The row's other cell: the flag, just inside the delete button. A click
/// runs the title bar's own flag action on the mail the row speaks for —
/// `Job::SetFlag` behind it — and is not a click on the row, so a mail is
/// marked where it is listed rather than opened.
#[test]
fn the_row_flag_button_marks_the_mail_it_lists() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
    let scheme = theme::of(&ctx);

    let at = row_flag_center(&app, 1);
    click_at(&ctx, &mut app, at);
    assert_eq!(
        app.selected_message, None,
        "the click flagged rather than opened the mail"
    );
    let messages = app.folder_messages_of("acct", "INBOX").expect("mail");
    let flags: Vec<bool> = messages.iter().map(|mail| mail.flagged).collect();
    assert!(flags[1], "the mail the row lists is flagged: {flags:?}");
    assert!(!flags[0] && !flags[2], "and only that one");

    // The pointer on another row: the flagged row takes the warning fill —
    // the pointer's own fill stands only while it is on *its* row — which is
    // the mark the cell put on the mail.
    let below = checkbox_center(&app, 2, false);
    frame_shapes_with(
        &ctx,
        &mut app,
        1400.0,
        vec![egui::Event::PointerMoved(egui::pos2(320.0, below.y))],
    );
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(
        fills_in_color(&shapes, scheme.flag)
            .iter()
            .any(|rect| rect.width() > 200.0),
        "the flagged row takes the warning fill"
    );

    // The pointer on the row itself, away from the cells: the flag button is
    // there — the pennant and its pole — muted and unfilled, the way the
    // delete button beside it is.
    let row = checkbox_center(&app, 1, false);
    frame_shapes_with(
        &ctx,
        &mut app,
        1400.0,
        vec![egui::Event::PointerMoved(egui::pos2(320.0, row.y))],
    );
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(
        fills_of_size(&shapes, egui::Vec2::splat(ROW_ACTION_SIZE))
            .iter()
            .all(|fill| fill.rect.center() != at),
        "the cell is not filled until the pointer is on it"
    );
    let cell = egui::Rect::from_center_size(at, egui::Vec2::splat(ROW_ACTION_SIZE));
    assert_eq!(
        strokes_in(&shapes, cell).len(),
        2,
        "the pennant and its pole stand on the cell"
    );

    // The pointer on the cell: the warning tint under the glyph, which is
    // what the click puts on the mail.
    frame_shapes_with(&ctx, &mut app, 1400.0, vec![egui::Event::PointerMoved(at)]);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(
        fills_of_size(&shapes, egui::Vec2::splat(ROW_ACTION_SIZE))
            .iter()
            .any(|fill| fill.rect.center() == at && fill.fill == scheme.flag),
        "the cell takes the warning tint under the pointer"
    );
}

/// The same cell takes the mark off a flagged mail, and says so — the title
/// bar's own wording, which turns over with the state; a conversation's row
/// names the newest mail it marks, the way its delete button beside it does.
#[test]
fn the_row_flag_button_takes_the_mark_off_and_says_which_mail() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    app.set_flag_on_mail(&MailKey::new("acct", "INBOX", "1"), true);
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

    let at = row_flag_center(&app, 1);
    click_at(&ctx, &mut app, at);
    assert!(
        !app.folder_messages_of("acct", "INBOX").expect("mail")[1].flagged,
        "the second click takes the flag off"
    );

    assert_eq!(
        row_flag_tooltip(RowPlace::Plain, false),
        MailAction::tooltip(MailAction::Flag)
    );
    assert_eq!(
        row_flag_tooltip(RowPlace::InThread, true),
        MailAction::flag_tooltip(true),
        "a mail of a thread says what the click takes off"
    );
    let thread = row_flag_tooltip(
        RowPlace::Thread {
            open: false,
            count: 2,
            key: "1",
        },
        false,
    );
    assert!(
        thread.contains("newest mail"),
        "a conversation's cell says which mail it marks: {thread}"
    );
}

/// A conversation's row speaks for its newest mail, and its flag cell marks
/// that one — not the thread: the mails under it are their own rows' to mark.
#[test]
fn a_conversations_flag_button_marks_the_mail_its_row_speaks_for() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_one_conversation(&ctx);
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

    let at = row_flag_center(&app, 0);
    click_at(&ctx, &mut app, at);
    let messages = app.folder_messages_of("acct", "INBOX").expect("mail");
    let flagged: Vec<&str> = messages
        .iter()
        .filter(|mail| mail.flagged)
        .map(|mail| mail.uid.as_str())
        .collect();
    assert_eq!(
        flagged,
        ["3"],
        "the newest mail of the thread, and nothing under it"
    );
}

/// The floating `Load older mail` button: drawn only while the folder's
/// marks say the server has mail below everything this machine holds, and
/// only at the bottom of the list — the one place where "older" is what the
/// eye is on. A list shorter than its viewport is at its bottom by
/// definition, which is how the first two frames below are drawn.
#[test]
fn the_load_older_button_is_offered_at_the_bottom_of_the_list() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);

    // The marks have not been asked yet: nothing to load, nothing drawn.
    assert!(!frame_texts(&ctx, &mut app)
        .iter()
        .any(|text| text == "Load older mail"));

    // The server said there is older mail (as a sync would).
    let folder = app
        .states
        .get_mut("acct")
        .unwrap()
        .mailbox
        .folders
        .iter_mut()
        .find(|folder| folder.name == "INBOX")
        .unwrap();
    folder.can_load_older = true;
    assert!(frame_texts(&ctx, &mut app)
        .iter()
        .any(|text| text == "Load older mail"));

    // A long list starts at its top, which is not the bottom.
    let mut long = app_with_messages(&ctx, 60);
    long.states
        .get_mut("acct")
        .unwrap()
        .mailbox
        .folders
        .iter_mut()
        .find(|folder| folder.name == "INBOX")
        .unwrap()
        .can_load_older = true;
    run_app_frame(&ctx, &mut long, Vec::new());
    assert!(
        !frame_texts(&ctx, &mut long)
            .iter()
            .any(|text| text == "Load older mail"),
        "the button belongs to the bottom of the list, not to its top"
    );

    // Scrolled all the way down, it is there. The wheel only reaches a
    // scroll area the pointer is over.
    run_app_frame(
        &ctx,
        &mut long,
        vec![egui::Event::PointerMoved(egui::pos2(300.0, 400.0))],
    );
    run_app_frame(
        &ctx,
        &mut long,
        vec![egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -100_000.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::default(),
        }],
    );
    for _ in 0..30 {
        run_app_frame(&ctx, &mut long, Vec::new());
    }
    assert!(long.list_scroll > 0.0, "the list did not scroll");
    assert!(
        frame_texts(&ctx, &mut long)
            .iter()
            .any(|text| text == "Load older mail"),
        "the end of the list is where the button is offered"
    );
}

/// Where the bar's two anchored groups go, for every window shape that
/// matters: the box is the message list's, the run the reading pane's, both
/// clamped clear of the first group — and the run is given up rather than
/// drawn over the list when the pane has no room for it beside the window
/// controls.
#[test]
fn the_bar_anchors_the_list_and_the_pane() {
    let bar = |width: f32| {
        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, TITLEBAR_HEIGHT))
    };
    let first_end = first_group_end();
    let width = titlebar_cells_width(MailAction::ALL.len());
    // What the bar's right end takes: the three window controls, the cog and
    // the sync cell, with the gaps between them and the edge margin.
    let cluster_left = |window: f32| titlebar_rects(bar(window), 3).sync.left();
    // What that right end takes off a window's width, so the case below can
    // build a bar out of the room the box needs rather than a hand-tuned
    // number that has to follow every size the bar's left group grows by.
    let right_end = 1400.0 - cluster_left(1400.0);

    // The default window (1400 pt, a 200 pt navigation column and a 460 pt
    // list): the box is the list's own edge, the run the pane's — and the
    // 740 pt pane has room for the run at that edge, clear of the window
    // controls.
    let anchors = titlebar_anchors(bar(1400.0), first_end, 200.0, 660.0, cluster_left(1400.0));
    assert_eq!(anchors.search, 200.0, "the box is the list's left edge");
    let actions = anchors.actions.expect("the run has room at 1400 pt");
    assert_eq!(actions.width(), width);
    assert_eq!(actions.left(), 660.0, "the run is the pane's own row");
    assert!(
        actions.right() + MAIL_ACTION_EDGE_GAP <= cluster_left(1400.0) + 0.5,
        "the run stays clear of the window controls: {actions:?}"
    );

    // A window 1110 pt wide: its 450 pt pane leaves no room for the run at
    // its edge beside the controls, so the run steps left over that edge —
    // by no more than the card's own margin.
    let anchors = titlebar_anchors(bar(1110.0), first_end, 200.0, 660.0, cluster_left(1110.0));
    let actions = anchors
        .actions
        .expect("the run still fits beside the controls");
    assert_eq!(actions.right(), cluster_left(1110.0) - MAIL_ACTION_EDGE_GAP);
    assert!(
        actions.left() >= 660.0 - MAIL_ACTION_OVERHANG,
        "the run hangs {} pt over the pane's edge, further than the card's margin",
        660.0 - actions.left()
    );

    // The filter cell is the box's own companion: a cell's gap to its right,
    // inside the room the box's plan left for it.
    let filter = anchors
        .filter
        .expect("the bar has room for the filter at 1400 pt");
    assert_eq!(anchors.search_width, Some(TITLEBAR_SEARCH_WIDTH));
    assert_eq!(
        filter.left(),
        anchors.search + TITLEBAR_SEARCH_WIDTH + TITLEBAR_BUTTON_GAP
    );
    assert_eq!(filter.width(), TITLEBAR_BUTTON_SIZE);
    assert!(
        filter.right() + TITLEBAR_GROUP_GAP <= cluster_left(1400.0) + 0.5,
        "the filter stays clear of the bar's right end: {filter:?}"
    );
    // And the sort cell follows it, one cell's gap further on: the two are
    // the box's group, in the order they are given up in.
    let sort = anchors
        .sort
        .expect("the bar has room for the sort at 1400 pt");
    assert_eq!(sort.left(), filter.right() + TITLEBAR_BUTTON_GAP);
    assert_eq!(sort.width(), TITLEBAR_BUTTON_SIZE);
    assert!(
        sort.right() + TITLEBAR_GROUP_GAP <= cluster_left(1400.0) + 0.5,
        "the sort stays clear of the bar's right end: {sort:?}"
    );

    // A bar with room for the filter but not for the sort after it: the
    // filter is the one that stays, and the sort is not drawn at all. The
    // bar ends where the filter's own square does — the box's width, a
    // cell's gap and the filter, and a group's gap at either end.
    let cluster = first_end
        + TITLEBAR_GROUP_GAP
        + TITLEBAR_SEARCH_WIDTH
        + TITLEBAR_BUTTON_GAP
        + TITLEBAR_BUTTON_SIZE
        + TITLEBAR_GROUP_GAP;
    let anchors = titlebar_anchors(bar(cluster + right_end), first_end, 0.0, 0.0, cluster);
    assert_eq!(anchors.search_width, Some(TITLEBAR_SEARCH_WIDTH));
    assert!(anchors.filter.is_some(), "the filter still fits");
    assert!(
        anchors.sort.is_none(),
        "and the sort does not: {:?}",
        anchors.sort
    );

    // A bar with room for the box but none for a cell beside it: the box is
    // what is kept — it is the list's own, and its width is planned first —
    // and neither cell is drawn rather than squeezed or drawn over its
    // neighbours. The bar is the box's own width and the two group gaps
    // around it and nothing more: a cell would need a square on top of that.
    let cluster = first_end + TITLEBAR_GROUP_GAP + TITLEBAR_SEARCH_WIDTH + TITLEBAR_GROUP_GAP;
    let anchors = titlebar_anchors(bar(cluster + right_end), first_end, 0.0, 0.0, cluster);
    assert_eq!(anchors.search_width, Some(TITLEBAR_SEARCH_WIDTH));
    assert!(
        anchors.filter.is_none(),
        "no room for a cell beside the box: {:?}",
        anchors.filter
    );
    assert!(anchors.sort.is_none(), "nor for the one after it");

    // A collapsed navigation column puts the list at the window's edge: the
    // box follows it and stops clear of the first group instead of running
    // under it.
    let anchors = titlebar_anchors(bar(1400.0), first_end, 0.0, 460.0, cluster_left(1400.0));
    assert_eq!(anchors.search, first_end + TITLEBAR_GROUP_GAP);

    // A narrow pane: too little room beside the window controls for the run,
    // and too far from the pane for it to be the pane's. The mail's own card
    // head carries the actions there.
    let anchors = titlebar_anchors(
        bar(1400.0),
        first_end,
        200.0,
        1400.0 - CONTENT_MIN_WIDTH,
        cluster_left(1400.0),
    );
    assert!(
        anchors.actions.is_none(),
        "a {} pt pane is no room for the run: {:?}",
        CONTENT_MIN_WIDTH,
        anchors.actions
    );
    assert!(
        anchors.filter.is_some(),
        "the box's companion is not the pane's: it survives where the run cannot"
    );

    // A window whose pane has no room for the run at all (the navigation
    // collapsed, the list at its minimum) still has room for the filter: it
    // is the box's, and it is the last of the middle band to go.
    let anchors = titlebar_anchors(bar(600.0), first_end, 0.0, 240.0, cluster_left(600.0));
    assert!(anchors.actions.is_none());
    assert_eq!(anchors.search_width, Some(TITLEBAR_SEARCH_WIDTH));
    assert!(
        anchors.filter.is_some(),
        "a 600 pt window still holds the box and its filter"
    );

    // The popup layout: the pane is the whole window, so its edge is behind
    // the first group and the run is the first thing after it.
    let anchors = titlebar_anchors(bar(1400.0), first_end, 0.0, 0.0, cluster_left(1400.0));
    assert_eq!(
        anchors.actions.map(|rect| rect.left()),
        Some(first_end + TITLEBAR_GROUP_GAP)
    );
    assert_eq!(anchors.search, first_end + TITLEBAR_GROUP_GAP);

    // And a window too narrow for the run and the controls together: at
    // 500 pt the sync cell starts at 306, and the run needs 266 pt of the
    // 174 that are left between the first group and that cell.
    assert!(
        titlebar_anchors(bar(500.0), first_end, 0.0, 0.0, cluster_left(500.0))
            .actions
            .is_none(),
        "a 500 pt window has no room for the run beside the controls"
    );
    // Nothing ever overlaps the first group, whatever the anchors were
    // handed — including a pane whose edge is left of it.
    for pane_left in [0.0, 40.0, 104.0, 200.0, 900.0, 1300.0] {
        let anchors =
            titlebar_anchors(bar(1400.0), first_end, 0.0, pane_left, cluster_left(1400.0));
        assert!(anchors.search >= first_end + TITLEBAR_GROUP_GAP);
        if let Some(filter) = anchors.filter {
            assert!(
                filter.left() >= first_end + TITLEBAR_GROUP_GAP,
                "{filter:?}"
            );
        }
        if let Some(sort) = anchors.sort {
            assert!(sort.left() >= first_end + TITLEBAR_GROUP_GAP, "{sort:?}");
        }
        if let Some(actions) = anchors.actions {
            assert!(
                actions.left() >= first_end + TITLEBAR_GROUP_GAP,
                "{actions:?}"
            );
            assert!(
                actions.right() + MAIL_ACTION_EDGE_GAP <= cluster_left(1400.0) + 0.5,
                "{actions:?}"
            );
        }
    }
}

/// The mail actions are the reading pane's, and they wait for a mail:
/// nothing while the pane is empty — not even dimmed cells — the run the
/// moment one is open, and nothing again when the pane is too narrow to hold
/// it beside the window controls, where the mail's own card head is what
/// carries the actions. Select mode is the exception to the first rule:
/// Delete takes the selection, so the run is there while something is
/// checked.
#[test]
fn the_mail_actions_are_the_panes_and_wait_for_a_mail() {
    /// Enough frames for the run's fade (`MAIL_ACTION_FADE_TIME`) to have
    /// finished at the `1/60` s a headless frame advances by.
    const FADE_FRAMES: usize = 16;

    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    let pitch = TITLEBAR_BUTTON_SIZE + TITLEBAR_BUTTON_GAP;
    let cell = |actions: egui::Rect, index: usize| {
        egui::pos2(
            actions.left() + index as f32 * pitch + TITLEBAR_BUTTON_SIZE / 2.0,
            TITLEBAR_HEIGHT / 2.0,
        )
    };
    let index_of = |action: MailAction| {
        MailAction::ALL
            .iter()
            .position(|candidate| *candidate == action)
            .expect("one of the seven")
    };

    // No mail open: the pane has room for the run and the bar draws none of
    // it — the first cell's place is chrome, not a button.
    let actions = mail_action_rect(&ctx, &mut app, 1400.0).expect("the pane has room");
    assert!(
        fill_under_the_pointer(
            &ctx,
            &mut app,
            1400.0,
            cell(actions, 0),
            TITLEBAR_BUTTON_SIZE
        )
        .is_none(),
        "no mail open, no run"
    );

    // A mail open: the run fades in, and it is the bar's own cells.
    app.selected_message = Some("0".to_string());
    for _ in 0..FADE_FRAMES {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    let filled = fill_under_the_pointer(
        &ctx,
        &mut app,
        1400.0,
        cell(actions, 0),
        TITLEBAR_BUTTON_SIZE,
    )
    .unwrap_or_else(|| panic!("the run is there with a mail open"));
    assert_eq!(filled.rect.center(), cell(actions, 0));

    // A window whose pane has no room for the run beside the window
    // controls — narrow enough that the navigation has collapsed and the
    // list is at its minimum, so the pane is what is left — gives the run
    // up: not one of its cells is drawn where the run would have been.
    const NARROW: f32 = 600.0;
    app.selected_message = None;
    // The navigation column collapses on the way into a window this narrow,
    // and the columns settle over the frames that follow: what the run has
    // to fit into is the plan the window has *after* that, not the one the
    // frame it arrived on was drawn from.
    for _ in 0..FADE_FRAMES {
        run_app_frame_sized(&ctx, &mut app, Vec::new(), egui::vec2(NARROW, 900.0));
    }
    assert!(
        mail_action_rect(&ctx, &mut app, NARROW).is_none(),
        "a {NARROW} pt window has no room for the run beside the controls"
    );
    app.selected_message = Some("0".to_string());
    for _ in 0..FADE_FRAMES {
        run_app_frame_sized(&ctx, &mut app, Vec::new(), egui::vec2(NARROW, 900.0));
    }
    let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(NARROW, TITLEBAR_HEIGHT));
    let rects = titlebar_rects(bar, 3);
    let packed =
        rects.sync.left() - MAIL_ACTION_EDGE_GAP - titlebar_cells_width(MailAction::ALL.len());
    for index in 0..MailAction::ALL.len() {
        let unheld = egui::pos2(
            packed + index as f32 * pitch + TITLEBAR_BUTTON_SIZE / 2.0,
            TITLEBAR_HEIGHT / 2.0,
        );
        assert!(
            fill_under_the_pointer(&ctx, &mut app, NARROW, unheld, TITLEBAR_BUTTON_SIZE).is_none(),
            "the run is not drawn where the pane cannot hold it: {unheld:?}"
        );
    }
    // The probe asks the right question: the bar's own cells are there at
    // that width, so it is the run that is missing and not the pointer.
    assert!(
        fill_under_the_pointer(
            &ctx,
            &mut app,
            NARROW,
            egui::pos2(
                TITLEBAR_LEFT_INSET + TITLEBAR_BUTTON_SIZE / 2.0,
                TITLEBAR_HEIGHT / 2.0
            ),
            TITLEBAR_BUTTON_SIZE
        )
        .is_some(),
        "the first group's cells are still the bar's"
    );

    // Select mode, nothing open in the pane, a mail checked: Delete is what
    // the selection is for, so the run is there — and it is the only cell of
    // the seven that answers.
    app.selected_message = None;
    app.select_mode = true;
    app.selected_mails
        .insert(MailKey::new("acct", "INBOX", "1"));
    for _ in 0..FADE_FRAMES {
        run_app_frame_sized(&ctx, &mut app, Vec::new(), egui::vec2(1400.0, 900.0));
    }
    let actions = mail_action_rect(&ctx, &mut app, 1400.0).expect("the pane has room");
    assert!(
        fill_under_the_pointer(
            &ctx,
            &mut app,
            1400.0,
            cell(actions, index_of(MailAction::Delete)),
            TITLEBAR_BUTTON_SIZE
        )
        .is_some(),
        "Delete is there for the selection"
    );
    assert!(
        fill_under_the_pointer(
            &ctx,
            &mut app,
            1400.0,
            cell(actions, index_of(MailAction::Reply)),
            TITLEBAR_BUTTON_SIZE
        )
        .is_none(),
        "the rest are about the mail the pane shows"
    );
}

/// The paper cell: the sixth of the head's run, and the one control there
/// that is about the view rather than about the mail. A click flips that one
/// mail's paper — the mail beside it keeps its own — and costs no render: the
/// page in the cache is the same page, drawn on the other paper.
#[test]
fn the_paper_cell_flips_one_mails_paper_and_nothing_else() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 2);
    html_body(&mut app, 0);
    html_body(&mut app, 1);

    let page = egui::vec2(400.0, 200.0);
    let (shapes, _) = blitz_frame_with_page(&ctx, &mut app, "0", page);
    assert!(paper_fill(&shapes, page, egui::Color32::WHITE));

    let pages = app.blitz_pages.len();
    click_at(&ctx, &mut app, header_action_center(5));
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(
        paper_fill(&shapes, page, BLITZ_PAPER_DARK),
        "the click drew this mail on the dark surface"
    );
    assert!(
        !paper_fill(&shapes, page, egui::Color32::WHITE),
        "and the white paper is gone"
    );
    assert_eq!(
        app.blitz_pages.len(),
        pages,
        "the flip repainted the frame and started no render"
    );

    // The mail next to it is its own: the paper is per mail.
    let (shapes, _) = blitz_frame_with_page(&ctx, &mut app, "1", page);
    assert!(paper_fill(&shapes, page, egui::Color32::WHITE));

    // And the first keeps what it was given, for the session.
    let (shapes, _) = blitz_frame_with_page(&ctx, &mut app, "0", page);
    assert!(paper_fill(&shapes, page, BLITZ_PAPER_DARK));

    // Clicking again puts it back on the default, which is not a record.
    click_at(&ctx, &mut app, header_action_center(5));
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(paper_fill(&shapes, page, egui::Color32::WHITE));
    assert!(
        app.blitz_paper.is_empty(),
        "white is the default, not an entry"
    );
}

/// The paper map holds only the mails the reader has changed, and loses an
/// account's entries with that account's pages.
#[test]
fn the_paper_map_holds_only_what_the_reader_changed() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    let key = MailKey::new("acct", "INBOX", "0");

    assert_eq!(app.paper_of(&key), Paper::White, "white is the default");
    app.toggle_blitz_paper(&key);
    assert_eq!(app.paper_of(&key), Paper::Dark);
    assert_eq!(app.blitz_paper.len(), 1);
    app.toggle_blitz_paper(&key);
    assert_eq!(app.paper_of(&key), Paper::White);
    assert!(
        app.blitz_paper.is_empty(),
        "a mail switched back takes its entry with it"
    );

    app.toggle_blitz_paper(&key);
    app.forget_blitz_pages("acct");
    assert!(
        app.blitz_paper.is_empty(),
        "an account's papers go with its pages"
    );

    assert_eq!(Paper::White.color(), egui::Color32::WHITE);
    assert_eq!(Paper::Dark.color(), BLITZ_PAPER_DARK);
    assert_ne!(Paper::White.tooltip(), Paper::Dark.tooltip());
}

#[test]
fn quantize_rounds_up_to_geometry_steps() {
    assert_eq!(quantize(0.0, 32), 32);
    assert_eq!(quantize(32.0, 32), 32);
    assert_eq!(quantize(32.1, 32), 64);
    assert_eq!(quantize(1000.0, 128), 1024);
    assert_eq!(quantize(f32::NAN, 32), 32);
}

#[test]
fn gtk_dark_flag_parsing() {
    assert!(ini_flag_is_true(
        "[Settings]\ngtk-application-prefer-dark-theme=true\n",
        "gtk-application-prefer-dark-theme"
    ));
    assert!(ini_flag_is_true(
        "[Settings]\ngtk-application-prefer-dark-theme = 1\n",
        "gtk-application-prefer-dark-theme"
    ));
    assert!(!ini_flag_is_true(
        "[Settings]\ngtk-application-prefer-dark-theme=0\n",
        "gtk-application-prefer-dark-theme"
    ));
    assert!(!ini_flag_is_true(
        "[Settings]\n",
        "gtk-application-prefer-dark-theme"
    ));
}

#[test]
fn kde_dark_scheme_parsing() {
    assert!(kde_scheme_is_dark("[General]\nColorScheme=BreezeDark\n"));
    assert!(!kde_scheme_is_dark("[General]\nColorScheme=BreezeLight\n"));
    assert!(!kde_scheme_is_dark("[General]\n"));
}

/// A sign-in that fails puts the form back on the account with the
/// reason beside the button that was clicked — the outcome belongs where
/// the click happened, not on a page the window already left.
#[test]
fn a_failed_sign_in_reopens_the_form_with_its_error() {
    let ctx = egui::Context::default();
    let mut account = test_account("imap.example.com");
    account.provider = Some("gmail".to_string());
    let mut app = KimoApp::assemble(
        &ctx,
        vec![account],
        std::collections::HashMap::new(),
        &storage::Settings::default(),
        std::sync::Arc::new(crate::plugin::discovery::Registry::empty()),
    );
    app.plugin_signing_in = Some("work".to_string());
    app.show_account_form = false;
    app.account_error = None;

    app.apply_event(sync::Event::PluginAuth {
        account_id: "work".to_string(),
        error: Some("config: no client id".to_string()),
    });

    assert!(app.show_account_form, "the form comes back");
    assert_eq!(app.account_form.editing.as_deref(), Some("work"));
    assert_eq!(app.account_error.as_deref(), Some("config: no client id"));
    assert!(app.plugin_signing_in.is_none(), "the wait is over");
}

/// A finished sign-in ends the wait even where the database is not there
/// to say more: the form it waited on closes, nothing is left looking
/// like a sign-in still in flight.
#[test]
fn a_finished_sign_in_ends_the_wait() {
    let ctx = egui::Context::default();
    let mut account = test_account("imap.example.com");
    account.provider = Some("outlook".to_string());
    let mut app = KimoApp::assemble(
        &ctx,
        vec![account],
        std::collections::HashMap::new(),
        &storage::Settings::default(),
        std::sync::Arc::new(crate::plugin::discovery::Registry::empty()),
    );
    app.plugin_signing_in = Some("work".to_string());
    app.show_account_form = true;
    app.account_form.editing = Some("work".to_string());

    app.apply_event(sync::Event::PluginAuth {
        account_id: "work".to_string(),
        error: None,
    });

    assert!(app.plugin_signing_in.is_none());
    assert!(
        !app.show_account_form,
        "a signed-in form has nothing left to ask"
    );
    assert!(app.account_error.is_none());
}

#[test]
fn renaming_or_retyping_a_password_keeps_the_cached_mail() {
    let account = test_account("imap.example.com");
    let mut form = AccountForm::edit(&account);
    form.name = "Home mail".to_string();
    form.password = "a new secret".to_string();
    assert!(!form.points_elsewhere(&account));
}

#[test]
fn a_plugin_form_needs_only_a_name_and_an_address() {
    let mut form = AccountForm::default();
    assert!(!form.is_valid());
    form.plugin = Some("gmail".to_string());
    form.name = "Work".to_string();
    assert!(!form.is_valid(), "an address is still required");
    form.username = "me@gmail.com".to_string();
    assert!(form.is_valid(), "the module owns every endpoint");
}

#[test]
fn another_endpoint_is_another_mailbox() {
    let account = test_account("imap.example.com");
    let mut form = AccountForm::edit(&account);
    form.host = "imap.other.example".to_string();
    assert!(form.points_elsewhere(&account));

    let mut form = AccountForm::edit(&account);
    form.port_str = "143".to_string();
    assert!(form.points_elsewhere(&account));

    let mut form = AccountForm::edit(&account);
    form.username = "someone@example.com".to_string();
    assert!(form.points_elsewhere(&account));

    let mut form = AccountForm::edit(&account);
    form.protocol = Protocol::Pop3;
    assert!(form.points_elsewhere(&account));

    // Trailing space is trimmed, not a different server.
    let mut form = AccountForm::edit(&account);
    form.host = "  imap.example.com  ".to_string();
    assert!(!form.points_elsewhere(&account));
}

#[test]
fn slug_sanitizes_names() {
    assert_eq!(slug("Gmail 主力"), "gmail");
    assert_eq!(slug("  Work! Mail "), "work-mail");
    assert_eq!(slug("???"), "account");
}

#[test]
fn truncate_galley_cuts_to_fit_and_appends_ellipsis() {
    let ctx = egui::Context::default();
    // epaint panics on a dropped `TexturesDelta` (egui 0.36).
    let mut frame = ctx.run_ui(egui::RawInput::default(), |ui| {
        let color = egui::Color32::WHITE;
        let short = truncate_galley(ui.ctx(), "hi", 14.0, color, 500.0);
        assert_eq!(short.text(), "hi");

        let long = truncate_galley(
            ui.ctx(),
            &"很长的一段中文内容。".repeat(10),
            14.0,
            color,
            100.0,
        );
        assert!(long.size().x <= 100.0);
        assert!(long.elided, "the text should say it was cut");
        assert_eq!(long.rows.len(), 1, "the row should stay a single line");
        assert_eq!(
            long.rows[0].glyphs.last().map(|glyph| glyph.chr),
            Some('…'),
            "the row should end in the ellipsis"
        );
    });
    frame.textures_delta.clear();
}

#[test]
fn timestamp_formatting() {
    // 2026-10-09T00:00:00Z
    assert_eq!(format_timestamp(1_791_504_000), "2026-10-09 00:00");
}

#[test]
fn sorting_puts_newest_first() {
    let mut msgs = vec![
        MailMessage {
            uid: "1".into(),
            subject: String::new(),
            from: String::new(),
            to: String::new(),
            date: String::new(),
            timestamp: Some(100),
            seen: false,
            flagged: false,
            message_id: String::new(),
            in_reply_to: Vec::new(),
            references: Vec::new(),
            body_text: String::new(),
            body_html: None,
            preview: String::new(),
            attachments: vec![],
            inline_images: vec![],
            labels: BTreeSet::new(),
        },
        MailMessage {
            uid: "2".into(),
            subject: String::new(),
            from: String::new(),
            to: String::new(),
            date: String::new(),
            timestamp: Some(200),
            seen: false,
            flagged: false,
            message_id: String::new(),
            in_reply_to: Vec::new(),
            references: Vec::new(),
            body_text: String::new(),
            body_html: None,
            preview: String::new(),
            attachments: vec![],
            inline_images: vec![],
            labels: BTreeSet::new(),
        },
    ];
    sort_messages(&mut msgs);
    assert_eq!(msgs[0].uid, "2");
}

#[test]
fn the_result_count_says_when_it_is_a_floor() {
    assert_eq!(results_label(0), "0 results");
    assert_eq!(results_label(1), "1 result");
    assert_eq!(results_label(12), "12 results");
    // At the limit the database stopped asking, so the count is a floor.
    assert_eq!(results_label(SEARCH_LIMIT), "200+ results");
}

/// The folders whose new mail is announced and the ones that stay quiet:
/// the user's own copies, the mailboxes mail is filtered into, and the
/// ones where a server shows every mail of the account again.
#[test]
fn only_delivered_mail_is_announced() {
    for kind in [FolderKind::Inbox, FolderKind::Archive, FolderKind::Folder] {
        assert!(announces_new_mail(kind), "{kind:?} is delivered mail");
    }
    for kind in [
        FolderKind::Sent,
        FolderKind::Drafts,
        FolderKind::Trash,
        FolderKind::Junk,
        FolderKind::AllMail,
        FolderKind::Starred,
    ] {
        assert!(!announces_new_mail(kind), "{kind:?} is not news");
    }
}

/// The header's delete cell takes the one mail whose header it is on: a
/// delete job is what the click starts — the same `Job::DeleteMessage`
/// the row's own button and the title bar's Delete send, which is
/// bracketed by `SyncStarted` and reaches a server the fixture does not
/// have. That the job started is what is observable here.
#[test]
fn the_headers_delete_cell_takes_that_one_mail() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    app.selected_message = Some("1".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());
    while app.rx.try_recv().is_ok() {}

    click_at(&ctx, &mut app, header_action_center(0));
    assert!(
        sync_starts_within(&mut app, Duration::from_secs(5)),
        "the delete cell started a delete job"
    );
}

/// The popup form's fields are quiet, the way `docs/ui-style.md` asks: the
/// input fill and no line at rest, the accent line while the keyboard is in
/// one or the pointer is over it — and no line again once neither is. The
/// accent is the reader's to choose, so the same card is drawn in whatever
/// colour they picked — here Teal, which neither scheme ships.
#[test]
fn a_quiet_field_wears_its_line_only_under_the_pointer_or_the_keyboard() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.accent = Accent::parse("teal");
    // A new mail in the popup: its form's fields are the quiet ones, and its
    // first frame puts the keyboard in To. The frames are what the modal's
    // own fade needs: a shape drawn while it is still coming in is a shape
    // at half its colour.
    app.compose = Some(ComposeDraft::new(Some("acct".to_string())));
    app.compose_spot = ComposeSpot::Popup;
    for _ in 0..FADE_FRAMES {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    let scheme = theme::of(&ctx);
    assert_eq!(
        scheme.accent,
        app.accent.colors(scheme.dark).accent,
        "the chosen accent is the one the app draws with"
    );

    // The form's single-line fields are the input fills inside the popup's
    // area, one field tall, top to bottom — the fill is what finds a box
    // whose line is not always there. To gives the fold chevron its cell,
    // which is why it is the narrower of the two.
    let fields = |shapes: &[egui::epaint::ClippedShape]| -> Vec<egui::Rect> {
        let mut rows: Vec<egui::Rect> = fills_in_color(shapes, scheme.input)
            .into_iter()
            .filter(|rect| popup_area().contains_rect(*rect) && rect.height() < 21.0)
            .collect();
        rows.sort_by(|a, b| a.top().total_cmp(&b.top()));
        rows
    };
    let line_of = |shapes: &[egui::epaint::ClippedShape], field: &egui::Rect| {
        stroked_boxes_in(shapes, popup_area())
            .iter()
            .find(|(rect, _)| rect == field)
            .map(|(_, color)| *color)
    };

    // To has the keyboard on the card's first frame: it wears the accent.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let rows = fields(&shapes);
    assert_eq!(
        rows.len(),
        2,
        "To and Subject, with Cc and Bcc folded away: {rows:?}"
    );
    let (to, subject) = (rows[0], rows[1]);
    assert!(
        to.width() < subject.width(),
        "To gives the fold chevron its cell: {to:?} of {subject:?}"
    );
    assert_eq!(
        line_of(&shapes, &to),
        Some(scheme.accent),
        "the field with the keyboard wears the accent"
    );
    // Subject waits: a quiet field with neither the pointer nor the keyboard
    // on it wears no line at all.
    assert_eq!(
        line_of(&shapes, &subject),
        None,
        "a waiting field wears no line"
    );

    // The pointer is what brings the line out, and it goes when the pointer
    // does; the keyboard's field keeps its own.
    let shapes = frame_shapes_with(
        &ctx,
        &mut app,
        1400.0,
        vec![egui::Event::PointerMoved(subject.center())],
    );
    assert_eq!(
        line_of(&shapes, &subject),
        Some(scheme.accent),
        "the pointer brings the line out"
    );
    let away = egui::pos2(10.0, 880.0);
    let shapes = frame_shapes_with(
        &ctx,
        &mut app,
        1400.0,
        vec![egui::Event::PointerMoved(away)],
    );
    assert_eq!(
        line_of(&shapes, &subject),
        None,
        "and the line goes with the pointer"
    );
    assert_eq!(
        line_of(&shapes, &to),
        Some(scheme.accent),
        "the keyboard's line stays while it has the keyboard"
    );
}

/// The filter cell sits in the search box's own group and its menu is the
/// views of the list as checkboxes — an AND between them, so ticking one
/// never takes another off and unticking one widens the list again — with
/// the "All" row as the way out of every view at once.
#[test]
fn the_filter_cell_narrows_the_list() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    // Mail "0" is unread, flagged and carries a file; "1" is read; "2" is
    // plain unread. The three marks between them are what the views are
    // about, and the subjects are what the rows say.
    {
        let state = app.states.get_mut("acct").expect("the fixture account");
        let mails = &mut state.mailbox.folders[0].messages;
        mails[1].seen = true;
        mails[0].flagged = true;
        mails[0].attachments = vec![Attachment {
            name: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size: 1024,
        }];
        state.folders_changed();
    }
    let drawn =
        |texts: &[String], uid: &str| texts.iter().any(|run| run == &format!("Subject {uid}"));

    let texts = frame_texts(&ctx, &mut app);
    assert!(
        drawn(&texts, "0") && drawn(&texts, "1") && drawn(&texts, "2"),
        "every mail is in the list while no filter is on: {texts:?}"
    );

    // The cell opens the menu, which is the views as rows under their "All"
    // one. It is an `Area` like the label popup's: it lays itself out on the
    // frame the click landed on and draws on the next one, so the shapes the
    // rows are found in are the frame after that.
    let open_menu = |ctx: &egui::Context, app: &mut KimoApp| {
        let cell = titlebar_filter_rect(ctx, app, 1400.0);
        click_at(ctx, app, cell.center());
        let _ = frame_shapes_at(ctx, app, 1400.0);
        frame_shapes_at(ctx, app, 1400.0)
    };
    // The menu hangs under the cell, at the left of the window rather than
    // in the middle column: its rows are found where they are drawn — and
    // they stay there, because a tick does not close the menu.
    let menu_area = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
    let tick = |ctx: &egui::Context, app: &mut KimoApp, label: &str| {
        let shapes = frame_shapes_at(ctx, app, 1400.0);
        let row = text_boxes_of(&shapes, menu_area, label);
        assert_eq!(row.len(), 1, "the menu draws one {label} row");
        click_at(ctx, app, row[0].center());
    };
    let shapes = open_menu(&ctx, &mut app);
    assert_eq!(
        text_boxes_of(&shapes, menu_area, "All").len(),
        1,
        "the menu's way out of every view"
    );
    for filter in ListFilter::ALL {
        assert_eq!(
            text_boxes_of(&shapes, menu_area, filter.label()).len(),
            1,
            "the menu offers {}",
            filter.label()
        );
    }

    // A row is its view's own mark at the left, the name, and the tick at its
    // *end* — where a menu's check belongs — and nothing is ticked yet.
    let row_parts = |shapes: &[egui::epaint::ClippedShape], label: &str| {
        let text = text_boxes_of(shapes, menu_area, label)[0];
        // The row's own ends, measured from where its name starts: the mark
        // is the inset, the icon and the gap to the left of it, and the
        // popup's width reaches its right end.
        let row_left = text.left() - (FILTER_ROW_INSET + FILTER_ICON_SIZE + FILTER_ICON_GAP);
        let icon = egui::Rect::from_min_max(
            egui::pos2(row_left, text.top() - 4.0),
            egui::pos2(text.left() - 2.0, text.bottom() + 4.0),
        );
        let tail = egui::Rect::from_min_max(
            egui::pos2(text.right() + 2.0, text.top() - 4.0),
            egui::pos2(row_left + LIST_MENU_WIDTH, text.bottom() + 4.0),
        );
        (icon, tail)
    };
    for filter in ListFilter::ALL {
        let (icon, tail) = row_parts(&shapes, filter.label());
        assert!(
            !glyph_colors_in(&shapes, icon).is_empty(),
            "the {} row carries a mark of its own",
            filter.label()
        );
        assert!(
            glyph_colors_in(&shapes, tail).is_empty(),
            "and no tick while it is off: {:?}",
            glyph_colors_in(&shapes, tail)
        );
    }

    // Unread: the read mail is not in the list any more.
    tick(&ctx, &mut app, "Unread");
    assert!(app.list_filters.contains(ListFilter::Unread));
    let scheme = theme::of(&ctx);
    // The menu is an `Area` that fades in, and a shape's colour carries that
    // fade: the tick's own colour is the one the frame after it draws.
    for _ in 0..16 {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let (_, tail) = row_parts(&shapes, "Unread");
    assert!(
        glyph_colors_in(&shapes, tail).contains(&scheme.accent),
        "the tick is at the end of the row, in the accent: {:?}",
        glyph_colors_in(&shapes, tail)
    );
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        drawn(&texts, "0") && drawn(&texts, "2") && !drawn(&texts, "1"),
        "the read mail is filtered out: {texts:?}"
    );

    // Flagged as well: the views are an AND, so what is left is the one mail
    // that is both — unread *and* flagged, which is also the one with a file.
    tick(&ctx, &mut app, "Flagged");
    assert!(
        app.list_filters.contains(ListFilter::Flagged)
            && app.list_filters.contains(ListFilter::Unread)
    );
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        drawn(&texts, "0") && !drawn(&texts, "1") && !drawn(&texts, "2"),
        "only the mail that passes both views is listed: {texts:?}"
    );

    // Unticking one widens the list again rather than switching views: the
    // flag is off, and what is left is the unread view — which the flagged
    // mail is still part of, with the other unread one.
    tick(&ctx, &mut app, "Flagged");
    assert!(!app.list_filters.contains(ListFilter::Flagged));
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        drawn(&texts, "0") && drawn(&texts, "2") && !drawn(&texts, "1"),
        "the unread view alone: the read mail is still out: {texts:?}"
    );

    // And "All" is the way out of every view at once.
    tick(&ctx, &mut app, "All");
    assert!(app.list_filters.is_all());
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        drawn(&texts, "0") && drawn(&texts, "1") && drawn(&texts, "2"),
        "every mail is back: {texts:?}"
    );
}

/// The sort cell sits after the filter and opens the menu's two groups: the
/// order — newest on top, or oldest — and the key — date, sender, subject.
/// Each is a choice of one, ticked at the end of the row in force, and the
/// list is drawn in the order the two add up to.
#[test]
fn the_sort_cell_orders_the_list() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    app.selected_message = None;
    // Three mails whose three keys all disagree, so that which one the list
    // was sorted by is what the rows say. By date: Zebra (300), Apple (200),
    // Mango (100); by sender: Cleo, Bob, Ada; by subject: Apple, Mango,
    // Zebra.
    {
        let state = app.states.get_mut("acct").expect("the fixture account");
        let mails = &mut state.mailbox.folders[0].messages;
        for (index, (ts, from, subject)) in [
            (100, "Cleo <cleo@example.com>", "Mango"),
            (200, "Ada <ada@example.com>", "Apple"),
            (300, "Bob <bob@example.com>", "Zebra"),
        ]
        .into_iter()
        .enumerate()
        {
            mails[index].timestamp = Some(ts);
            mails[index].from = from.to_string();
            mails[index].subject = subject.to_string();
        }
        state.folders_changed();
    }
    // What the list drew, in the order it drew it. Each subject is one run
    // per row — an unread row's text is struck twice, half a point apart, so
    // the frame draws it twice — and the pane has no mail of its own on it to
    // draw a third.
    let drawn = |texts: &[String]| -> Vec<String> {
        let mut order: Vec<String> = Vec::new();
        for run in texts {
            if matches!(run.as_str(), "Mango" | "Apple" | "Zebra") && !order.contains(run) {
                order.push(run.clone());
            }
        }
        order
    };

    let texts = frame_texts(&ctx, &mut app);
    assert_eq!(
        drawn(&texts),
        ["Zebra", "Apple", "Mango"],
        "the default: newest first, by date"
    );

    // The menu, in the two groups the cell promises.
    let menu_area = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
    let open_sort = |ctx: &egui::Context, app: &mut KimoApp| {
        let cell = titlebar_sort_rect(ctx, app, 1400.0);
        click_at(ctx, app, cell.center());
        let _ = frame_shapes_at(ctx, app, 1400.0);
        frame_shapes_at(ctx, app, 1400.0)
    };
    let click_row = |ctx: &egui::Context, app: &mut KimoApp, label: &str| {
        let shapes = frame_shapes_at(ctx, app, 1400.0);
        let row = text_boxes_of(&shapes, menu_area, label);
        assert_eq!(row.len(), 1, "the menu draws one {label} row");
        click_at(ctx, app, row[0].center());
    };
    let shapes = open_sort(&ctx, &mut app);
    for text in [
        "Sort order",
        "Sort by",
        "Newest on top",
        "Oldest on top",
        "Date",
        "From",
        "Subject",
    ] {
        assert_eq!(
            text_boxes_of(&shapes, menu_area, text).len(),
            1,
            "the menu draws {text}"
        );
    }
    for row in ["Newest on top", "Oldest on top", "Date", "From", "Subject"] {
        let text = text_boxes_of(&shapes, menu_area, row)[0];
        let icon = egui::Rect::from_min_max(
            egui::pos2(text.left() - 40.0, text.top() - 4.0),
            egui::pos2(text.left() - 2.0, text.bottom() + 4.0),
        );
        assert!(
            !glyph_colors_in(&shapes, icon).is_empty(),
            "the {row} row carries a mark of its own"
        );
    }

    // Oldest on top: the same three mails, the other way round.
    click_row(&ctx, &mut app, "Oldest on top");
    assert_eq!(app.list_sort.order, SortOrder::Oldest);
    let texts = frame_texts(&ctx, &mut app);
    assert_eq!(drawn(&texts), ["Mango", "Apple", "Zebra"], "oldest first");

    // By sender, with "newest on top" back on: the order group is what turns
    // the key round, so the names run Z to A.
    click_row(&ctx, &mut app, "Newest on top");
    click_row(&ctx, &mut app, "From");
    assert_eq!(app.list_sort.by, SortBy::From);
    let texts = frame_texts(&ctx, &mut app);
    assert_eq!(
        drawn(&texts),
        ["Mango", "Zebra", "Apple"],
        "Cleo, Bob, Ada: the reverse of the names"
    );

    // And oldest first: the names themselves.
    click_row(&ctx, &mut app, "Oldest on top");
    let texts = frame_texts(&ctx, &mut app);
    assert_eq!(drawn(&texts), ["Apple", "Zebra", "Mango"], "Ada, Bob, Cleo");

    // By subject, the last of the three keys.
    click_row(&ctx, &mut app, "Subject");
    assert_eq!(app.list_sort.by, SortBy::Subject);
    let texts = frame_texts(&ctx, &mut app);
    assert_eq!(drawn(&texts), ["Apple", "Mango", "Zebra"], "A to Z");

    // The row in force in each group carries the tick at its end, and the
    // others do not: the menu says what the list is doing.
    let scheme = theme::of(&ctx);
    for _ in 0..16 {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let tail = |label: &str| {
        let text = text_boxes_of(&shapes, menu_area, label)[0];
        let row_left = text.left() - (FILTER_ROW_INSET + FILTER_ICON_SIZE + FILTER_ICON_GAP);
        egui::Rect::from_min_max(
            egui::pos2(text.right() + 2.0, text.top() - 4.0),
            egui::pos2(row_left + LIST_MENU_WIDTH, text.bottom() + 4.0),
        )
    };
    for (row, ticked) in [
        ("Oldest on top", true),
        ("Subject", true),
        ("Newest on top", false),
        ("Date", false),
        ("From", false),
    ] {
        let colors = glyph_colors_in(&shapes, tail(row));
        assert_eq!(
            colors.contains(&scheme.accent),
            ticked,
            "the {row} row's tick: {colors:?}"
        );
    }
}

/// The sort the menu asks for is one comparison: the three keys, either way
/// round, and where the mail sits as the tie-break — so two mails equal by
/// the key keep the folder's order rather than swapping about.
#[test]
fn the_list_sort_compares_by_its_key_and_stays_stable() {
    let key = |timestamp, from: &'static str, subject: &'static str, position| SortKey {
        timestamp,
        from,
        subject,
        position,
    };
    let newest_date = ListSort::default();
    assert_eq!(newest_date.order, SortOrder::Newest);
    assert_eq!(newest_date.by, SortBy::Date);
    // Newest first: the later date runs before the earlier one, and a mail
    // whose date would not parse is the oldest there is.
    assert_eq!(
        newest_date.compare(key(Some(200), "a", "a", 0), key(Some(100), "a", "a", 1)),
        std::cmp::Ordering::Less
    );
    assert_eq!(
        newest_date.compare(key(None, "a", "a", 0), key(Some(1), "a", "a", 1)),
        std::cmp::Ordering::Greater
    );
    // The order group turns whatever key round.
    let oldest = ListSort {
        order: SortOrder::Oldest,
        ..ListSort::default()
    };
    assert_eq!(
        oldest.compare(key(Some(200), "a", "a", 0), key(Some(100), "a", "a", 1)),
        std::cmp::Ordering::Greater
    );
    // By sender and by subject: display names, compared as a reader would —
    // case-insensitively.
    let by_from = ListSort {
        by: SortBy::From,
        ..ListSort::default()
    };
    assert_eq!(
        by_from.compare(
            key(Some(1), "Ada <a@x>", "z", 0),
            key(Some(2), "bob <b@x>", "a", 1)
        ),
        std::cmp::Ordering::Greater,
        "newest first reverses the names too: Ada runs after bob"
    );
    let by_from_ascending = ListSort {
        order: SortOrder::Oldest,
        ..by_from
    };
    assert_eq!(
        by_from_ascending.compare(
            key(Some(1), "Ada <a@x>", "z", 0),
            key(Some(2), "bob <b@x>", "a", 1)
        ),
        std::cmp::Ordering::Less,
        "and oldest first is the names themselves, case-insensitively"
    );
    let by_subject = ListSort {
        by: SortBy::Subject,
        ..ListSort::default()
    };
    assert_eq!(
        by_subject.compare(key(Some(2), "a", "Apple", 0), key(Some(1), "b", "Mango", 1)),
        std::cmp::Ordering::Greater,
        "the same reversal, by subject"
    );
    let by_subject_ascending = ListSort {
        order: SortOrder::Oldest,
        ..by_subject
    };
    assert_eq!(
        by_subject_ascending.compare(key(Some(2), "a", "Apple", 0), key(Some(1), "b", "Mango", 1)),
        std::cmp::Ordering::Less,
        "Apple before Mango, oldest first"
    );
    // Equal by the key: where the mail sits decides, whichever way the sort
    // runs — which is what keeps the list still between two frames.
    for sort in [newest_date, oldest, by_from, by_subject] {
        assert_eq!(
            sort.compare(key(Some(5), "a", "a", 3), key(Some(5), "a", "a", 7)),
            std::cmp::Ordering::Less,
            "{sort:?}"
        );
    }
}

/// The four views ask a row the four things a list can answer, and "to me"
/// reads the `To` header as the sender wrote it — the address is somewhere
/// in it, whatever else is there — against the accounts' own addresses. The
/// set they go into is the AND the menu promises.
#[test]
fn the_list_filter_predicates() {
    let me = vec!["me@example.com".to_string()];
    let facts = |seen, flagged, to: &'static str, attachments| FilterFacts {
        seen,
        flagged,
        to,
        attachments,
    };
    assert!(ListFilter::Unread.matches(facts(false, false, "x@y", 0), &me));
    assert!(!ListFilter::Unread.matches(facts(true, false, "x@y", 0), &me));
    assert!(ListFilter::Flagged.matches(facts(true, true, "x@y", 0), &me));
    assert!(!ListFilter::Flagged.matches(facts(true, false, "x@y", 0), &me));
    assert!(ListFilter::HasFiles.matches(facts(true, false, "x@y", 2), &me));
    assert!(!ListFilter::HasFiles.matches(facts(true, false, "x@y", 0), &me));
    assert!(ListFilter::ToMe.matches(facts(true, false, "Ada <ME@example.com>", 0), &me));
    assert!(ListFilter::ToMe.matches(facts(true, false, "a@x, me@example.com", 0), &me));
    assert!(!ListFilter::ToMe.matches(facts(true, false, "someone@elsewhere.com", 0), &me));
    // An account with no address of its own leaves the question unanswerable
    // — which is what the menu row says rather than filtering to nothing.
    assert!(!ListFilter::ToMe.matches(facts(true, false, "me@example.com", 0), &[]));
    for filter in ListFilter::ALL {
        assert!(!filter.label().is_empty());
        assert!(!filter.note().is_empty());
    }

    // The set they go into is the AND the menu promises: an empty one is
    // every mail, each tick narrows it further, and unticking widens it
    // again without disturbing the views that are left.
    let mut filters = ListFilters::default();
    assert!(filters.is_all());
    assert!(filters.matches(facts(true, false, "x@y", 0), &me));
    filters.toggle(ListFilter::Unread);
    filters.toggle(ListFilter::HasFiles);
    assert!(!filters.is_all());
    assert!(filters.contains(ListFilter::Unread) && filters.contains(ListFilter::HasFiles));
    assert!(filters.matches(facts(false, false, "x@y", 1), &me));
    assert!(
        !filters.matches(facts(false, false, "x@y", 0), &me),
        "unread but with nothing attached"
    );
    assert!(
        !filters.matches(facts(true, false, "x@y", 1), &me),
        "attached but read"
    );
    filters.toggle(ListFilter::Unread);
    assert!(!filters.contains(ListFilter::Unread));
    assert!(filters.matches(facts(true, false, "x@y", 1), &me));
    // The set keeps the menu's order, whatever order the boxes were ticked.
    filters.toggle(ListFilter::ToMe);
    filters.toggle(ListFilter::Unread);
    assert_eq!(
        filters.0,
        vec![ListFilter::Unread, ListFilter::ToMe, ListFilter::HasFiles]
    );
    filters.clear();
    assert!(filters.is_all());
}

/// A view that hides every mail leaves the column saying so: the list is
/// empty because of the filter, which is not the same thing as a folder with
/// no mail in it.
#[test]
fn a_filter_that_matches_nothing_says_so() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 2);
    app.set_list_filters(ListFilters(vec![ListFilter::Flagged]));
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts
            .iter()
            .any(|run| run == "No mail matches this filter."),
        "the list says why it is empty: {texts:?}"
    );
    assert!(
        !texts.iter().any(|run| run == "Subject 0"),
        "and draws no row at all: {texts:?}"
    );

    // The same for a search's hits, which the column's filter narrows
    // without touching the query: it says the hit was filtered out, not that
    // nothing matched.
    app.set_list_filters(ListFilters::default());
    app.search.text = "report".to_string();
    app.search_hits = vec![SearchHit {
        account_id: "acct".to_string(),
        folder: "INBOX".to_string(),
        uid: "0".to_string(),
        subject: "Subject 0".to_string(),
        from: "Ada <ada@example.com>".to_string(),
        to: "someone@elsewhere.com".to_string(),
        date: "2026-01-01".to_string(),
        timestamp: Some(1_700_000_000),
        seen: false,
        flagged: false,
        attachments: 0,
        snippet: "a line with report in it".to_string(),
    }];
    app.set_list_filters(ListFilters(vec![ListFilter::HasFiles]));
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts.iter().any(|run| run == "No hit matches this filter."),
        "a filtered-out hit is not an empty query: {texts:?}"
    );
}

/// The title bar's flag cell is the one mail action with a state of its
/// own: a click marks the mail the pane shows, the mark is the warning fill
/// the mail's **row** takes — the reading pane is the mail's own surface and
/// is not tinted — the cell's own glyph turns the scheme's warning colour
/// while the flag stands, so the cell says which of the two things a click
/// on it would do, and the same click takes the flag off again.
#[test]
fn the_flag_cell_marks_the_mail_it_is_on() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    let scheme = theme::of(&ctx);
    // The warning fill the list draws, told apart from the chrome: a row is
    // the only thing in the column that tall.
    let row_fill = |shapes: &[egui::epaint::ClippedShape], pane_left: f32| {
        fills_in_color(shapes, theme::of(&ctx).flag)
            .into_iter()
            .find(|rect| rect.height() >= MESSAGE_ROW_HEIGHT && rect.center().x < pane_left)
    };

    // Mail "1" is the one to mark, and the pane opens on it: the cell is
    // about the mail the pane shows.
    app.selected_message = Some("1".to_string());
    // The run fades in with the mail it acts on (`MAIL_ACTION_FADE_TIME`),
    // and what is asked below is the cell at rest — the glyph's own colour
    // rather than a faded one.
    for _ in 0..16 {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    assert!(
        !app.selected_mail().expect("a mail is open").flagged,
        "a mail starts unflagged"
    );
    let cell = egui::Rect::from_center_size(
        titlebar_action_center(&ctx, &mut app, MailAction::Flag, 1400.0),
        egui::Vec2::splat(TITLEBAR_BUTTON_SIZE),
    );
    let pane_left = app.list_column.right();
    click_at(&ctx, &mut app, cell.center());
    assert!(
        app.selected_mail().expect("a mail is open").flagged,
        "the cell marked the mail the pane shows"
    );

    // The cell draws its glyph in the warning colour rather than in the
    // bar's text colour — and the pane is still the mail's own surface: the
    // mark is shown in the list, not painted on the mail.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let colors = glyph_colors_in(&shapes, cell);
    assert!(
        colors.contains(&scheme.warn),
        "the flag cell draws its glyph in the warning colour: {colors:?}"
    );
    assert!(
        fills_in_color(&shapes, scheme.flag)
            .into_iter()
            .all(|rect| rect.center().x < pane_left),
        "no fill of the warning colour is on the pane's side of the splitter"
    );

    // Another mail is open: the flagged one — right there in the list,
    // unselected — is the row that takes the warning fill.
    app.selected_message = Some("0".to_string());
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let row = row_fill(&shapes, pane_left).expect("the flagged mail's row is filled");
    assert_eq!(
        row.top(),
        TITLEBAR_HEIGHT + MESSAGE_ROW_HEIGHT,
        "and it is the flagged mail's row, not the open one's"
    );

    // The click is a toggle, and it is about the mail the pane shows: with
    // the flagged mail open again, the same cell takes the mark off.
    app.selected_message = Some("1".to_string());
    for _ in 0..16 {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    click_at(&ctx, &mut app, cell.center());
    assert!(
        !app.selected_mail().expect("a mail is open").flagged,
        "the cell took the flag off again"
    );
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(
        row_fill(&shapes, pane_left).is_none(),
        "and the warning fill went with it"
    );
}

/// A flagged mail's row carries the mark itself rather than only the fill:
/// the flag glyph at the end of its title line, in the mark's own warning
/// colour — and for a mail that also has parts, beside its paperclip rather
/// than on top of it, since the row has room for both.
#[test]
fn a_flagged_row_draws_the_flag_beside_its_paperclip() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 2);
    let scheme = theme::of(&ctx);
    let key = MailKey::new("acct", "INBOX", "1");
    // The pane opens on mail "0", so the row asked about — the second one —
    // is drawn without the selection's own fill.
    app.selected_message = Some("0".to_string());
    let row = |app: &KimoApp| {
        egui::Rect::from_min_size(
            egui::pos2(app.list_column.left(), TITLEBAR_HEIGHT + MESSAGE_ROW_HEIGHT),
            egui::vec2(app.list_column.width(), MESSAGE_ROW_HEIGHT),
        )
    };
    let marks = |app: &mut KimoApp| {
        let shapes = frame_shapes_at(&ctx, app, 1400.0);
        let row = row(app);
        let colors = glyph_colors_in(&shapes, row);
        let bounds = list_glyph_strokes(&shapes, app.list_column)
            .into_iter()
            .filter(|stroke| row.contains(stroke.center()))
            .fold(None::<egui::Rect>, |bounds, stroke| {
                Some(bounds.map_or(stroke, |b| b.union(stroke)))
            });
        (colors, bounds, row)
    };

    // Nothing marked: no flag glyph on the row at all.
    let (colors, bounds, _) = marks(&mut app);
    assert!(
        !colors.contains(&scheme.warn) && bounds.is_none(),
        "an unmarked row draws no flag: {colors:?} {bounds:?}"
    );

    // Marked: the flag is drawn, in the warning colour, at the end of the
    // row's title line.
    app.set_flag_on_mail(&key, true);
    let (colors, bounds, row) = marks(&mut app);
    assert!(
        colors.contains(&scheme.warn),
        "the flagged row draws its flag in the warning colour: {colors:?}"
    );
    let flag = bounds.expect("the flagged row draws a glyph");
    assert!(
        flag.width() > ROW_FLAG_WIDTH - 2.0
            && flag.width() <= ROW_FLAG_SIZE
            && flag.height() > ROW_FLAG_SIZE - 6.0
            && flag.height() <= ROW_FLAG_SIZE,
        "the flag fills the box the row reserves for it: {flag:?}"
    );
    assert!(
        flag.right() > row.right() - 30.0 && flag.right() <= row.right(),
        "the flag sits at the end of the title line: {flag:?} in {row:?}"
    );

    // A mail with a part as well: the clip is the outermost of the two, so
    // the flag's own room is a clip's width further in — and the row draws
    // both marks, each in its own colour.
    let state = app.states.get_mut("acct").expect("the fixture account");
    state.mailbox.folders[0].messages[1].attachments = vec![Attachment {
        name: "report.pdf".to_string(),
        content_type: "application/pdf".to_string(),
        size: 1024,
    }];
    state.folders_changed();
    let (colors, bounds, _) = marks(&mut app);
    assert!(
        colors.contains(&scheme.warn) && colors.contains(&scheme.text_weak),
        "the row draws both marks, each in its own colour: {colors:?}"
    );
    let both = bounds.expect("the row draws both glyphs");
    assert!(
        both.width() > flag.width() + ROW_CLIP_GAP,
        "the clip sits beside the flag rather than over it: {both:?} against {flag:?}"
    );
}
