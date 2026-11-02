use super::super::*;
use super::common::*;

/// A selection made with the mouse is what the body menu's Copy is about:
/// clicking it is a copy event sent before the body is laid out again, so
/// egui's label selection still has the range when the copy runs.
#[test]
fn a_body_selection_is_copied_by_the_context_menu() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    {
        let state = app.states.get_mut("acct").expect("the fixture account");
        let mail = &mut state.mailbox.folders[0].messages[0];
        mail.body_text = "Hello body".to_string();
        // No HTML: this is the plain-text fallback, one selectable label.
        mail.body_html = None;
        state.folders_changed();
    }
    app.selected_message = Some("0".to_string());
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let window = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
    let hello = text_boxes_of(&shapes, window, "Hello body")
        .into_iter()
        .next()
        .expect("the body draws its text");

    // Drag across the text, the way a selection is made.
    let start = egui::pos2(hello.left() + 1.0, hello.center().y);
    let end = egui::pos2(hello.right() - 1.0, hello.center().y);
    run_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(start)]);
    run_app_frame(
        &ctx,
        &mut app,
        vec![egui::Event::PointerButton {
            pos: start,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        }],
    );
    run_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(end)]);
    run_app_frame(
        &ctx,
        &mut app,
        vec![egui::Event::PointerButton {
            pos: end,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        }],
    );
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(
        ctx.with_plugin::<egui::text_selection::LabelSelectionState, _>(|selection| {
            selection.has_selection()
        })
        .unwrap_or(false),
        "the drag selected the label text"
    );

    right_click_at(&ctx, &mut app, hello.center());
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(
        ctx.with_plugin::<egui::text_selection::LabelSelectionState, _>(|selection| {
            selection.has_selection()
        })
        .unwrap_or(false),
        "opening the menu released the text selection"
    );
    let copy = text_boxes_of(&shapes, window, "Copy")
        .into_iter()
        .next()
        .expect("the body menu draws Copy");
    let output = click_at_output(&ctx, &mut app, copy.center());
    assert!(
        output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text.contains("Hello"))
        }),
        "the selected text reaches the clipboard: {:?}",
        output.platform_output.commands
    );
    assert!(
        ctx.with_plugin::<egui::text_selection::LabelSelectionState, _>(|selection| {
            selection.has_selection()
        })
        .unwrap_or(false),
        "using the menu released the text selection"
    );
}

/// The boxes select mode draws are the avatar's slot, and the avatar is what
/// they replace: a frame without the pointer is all circles, and the pointer
/// on a row trades exactly that row's circle for a box. Hovering is how the
/// mode is entered, so the way in is where the mail's mark already is.
#[test]
fn the_pointer_puts_a_checkbox_where_the_avatar_was() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);

    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let column = app.list_column;
    assert!(
        checkboxes_in(&shapes, column).is_empty(),
        "the avatars stand while the pointer is away"
    );
    assert_eq!(avatars_in(&shapes, column).len(), 3);

    // The pointer on the second row. It is moved in one frame and the shapes
    // are read from the next: egui hit-tests against the previous frame's
    // rects.
    let at = checkbox_center(&app, 1, false);
    frame_shapes_with(&ctx, &mut app, 1400.0, vec![egui::Event::PointerMoved(at)]);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let boxes = checkboxes_in(&shapes, column);
    assert_eq!(
        boxes.len(),
        1,
        "one box, over the row under the pointer: {boxes:?}"
    );
    assert_eq!(boxes[0].center(), at, "and it is in the avatar's slot");
    assert_eq!(
        avatars_in(&shapes, column).len(),
        2,
        "the row under the pointer gave its avatar up"
    );
}

/// A click on the box turns select mode on and checks the mail the row
/// stands for. From then on the boxes are not the pointer's business: every
/// row in the list carries one, and the avatars are gone with the mode.
#[test]
fn clicking_a_box_enters_select_mode_with_a_box_on_every_row() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
    let column = app.list_column;

    let at = checkbox_center(&app, 1, false);
    click_at(&ctx, &mut app, at);
    assert!(app.select_mode, "a click on a box is the way into the mode");
    assert!(
        app.selected_mails
            .contains(&MailKey::new("acct", "INBOX", "1")),
        "the box checks the mail its row stands for"
    );
    assert_eq!(
        app.selected_message, None,
        "and a click on the box is not a click on the mail"
    );

    // A second box adds to the selection rather than replacing it — which is
    // the whole point of the mode — and a click on a checked one takes that
    // mail out again without leaving the mode.
    let second = checkbox_center(&app, 2, false);
    click_at(&ctx, &mut app, second);
    assert_eq!(app.selected_mails.len(), 2, "two mails are checked now");
    assert!(
        app.selected_mails
            .contains(&MailKey::new("acct", "INBOX", "2")),
        "the box that was clicked is the mail that joined"
    );
    click_at(&ctx, &mut app, second);
    assert_eq!(
        app.selected_mails.len(),
        1,
        "and clicking it again unchecks it"
    );
    assert!(app.select_mode, "unchecking does not leave the mode");

    // Away from the rows, so what is drawn is the mode and not the hover.
    frame_shapes_with(
        &ctx,
        &mut app,
        1400.0,
        vec![egui::Event::PointerMoved(egui::pos2(1000.0, 500.0))],
    );
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert_eq!(row_boxes_in(&shapes, column).len(), 3, "one box per row");
    assert!(
        avatars_in(&shapes, column).is_empty(),
        "select mode has no avatars at all"
    );
    // Two filled boxes: the checked mail's, and the select-all row's, which
    // stands for the one mail of three that is in.
    assert_eq!(
        fills_in_color(&shapes, theme::of(&ctx).accent).len(),
        2,
        "the checked mail's box and the band's"
    );
}

/// In select mode the row itself is not the mail any more: a click on it —
/// anywhere but the box — is the way out of the mode, and it does not open
/// the mail under the pointer or move the reading pane either.
#[test]
fn a_click_on_the_row_leaves_select_mode_without_opening_the_mail() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    app.selected_message = Some("2".to_string());
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

    let at = checkbox_center(&app, 1, false);
    click_at(&ctx, &mut app, at);
    assert!(app.select_mode);
    assert!(!app.selected_mails.is_empty());

    // The row beside the box — the mail's text, well clear of both the box
    // and the select-all row the mode put above it.
    let row = checkbox_center(&app, 1, false);
    click_at(&ctx, &mut app, egui::pos2(320.0, row.y));
    assert!(!app.select_mode, "a click beside the box is the way out");
    assert!(app.selected_mails.is_empty(), "and the checks go with it");
    assert_eq!(
        app.selected_message.as_deref(),
        Some("2"),
        "the row's click opened nothing"
    );
}

/// A box says *what* it holds and not only that it holds something: a checked
/// box is the accent fill with the check drawn on it, and a conversation with
/// only part of its mail checked gets the dash instead. The two are different
/// drawings, which is what keeps a half-checked thread from reading as a
/// checked one.
#[test]
fn a_partly_checked_conversation_draws_a_dash_instead_of_a_check() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_one_conversation(&ctx);
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
    // Open the conversation, then check its oldest mail alone: the
    // conversation's own box is the one left in between.
    let key = app
        .list_rows
        .as_ref()
        .expect("the list derived its rows")
        .threads[0]
        .key
        .clone();
    app.open_threads.insert(key);
    app.open_rev += 1;
    app.select_mode = true;
    app.selected_mails
        .insert(MailKey::new("acct", "INBOX", "1"));

    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let column = app.list_column;
    let mut boxes = checkboxes_in(&shapes, column);
    boxes.sort_by(|a, b| a.top().total_cmp(&b.top()));
    assert_eq!(
        boxes.len(),
        4,
        "the select-all row, the conversation and its two mails: {boxes:?}"
    );

    // The band is the first box, and it is partial for the same reason the
    // conversation's is: one mail of the two is checked.
    let band = strokes_in(&shapes, boxes[0]);
    assert_eq!(
        band,
        vec![2],
        "the select-all row says some of the list is in: {band:?}"
    );
    let conversation = strokes_in(&shapes, boxes[1]);
    assert_eq!(
        conversation,
        vec![2],
        "a partly checked conversation draws the dash: {conversation:?}"
    );
    let checked = strokes_in(&shapes, boxes[2]);
    assert_eq!(
        checked,
        vec![3],
        "a checked mail draws the check: {checked:?}"
    );
    assert!(
        strokes_in(&shapes, boxes[3]).is_empty(),
        "and an unchecked one draws nothing: {:?}",
        strokes_in(&shapes, boxes[3])
    );
}

/// A checked row is filled the way the reading pane's own mail is: the box is
/// the precise mark, and the fill is what says at a glance how much of the
/// list a Delete is about. A half-checked conversation's own row is not
/// filled — the dash is what says it is partly in.
#[test]
fn a_checked_row_takes_the_selection_fill() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    app.select_mode = true;
    app.selected_mails
        .insert(MailKey::new("acct", "INBOX", "1"));

    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let selection = theme::of(&ctx).selection;
    let filled: Vec<egui::Rect> = fills_in_color(&shapes, selection)
        .into_iter()
        .filter(|rect| rect.height() >= MESSAGE_ROW_HEIGHT)
        .collect();
    assert_eq!(filled.len(), 1, "one checked row, one fill: {filled:?}");
    let row = filled[0];
    assert_eq!(
        row.top(),
        TITLEBAR_HEIGHT + SELECT_ALL_HEIGHT + MESSAGE_ROW_HEIGHT,
        "the filled row is the second one — the mail that is checked"
    );
}

/// Select mode is the list's business, not the reading pane's: the actions
/// that are about one mail — reply, reply all, forward, label, flag, export
/// — are disabled, and Delete, which takes the whole selection, is the one
/// left live while something is checked.
#[test]
fn select_mode_leaves_delete_as_the_only_live_action() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    app.selected_message = Some("0".to_string());
    assert!(
        app.mail_action_enabled(MailAction::Reply),
        "view mode: the pane's mail is what the seven act on"
    );

    app.select_mode = true;
    for action in MailAction::ALL {
        assert!(
            !app.mail_action_enabled(action),
            "nothing is checked yet, so {action:?} has nothing to act on"
        );
    }
    app.selected_mails
        .insert(MailKey::new("acct", "INBOX", "1"));
    for action in MailAction::ALL {
        assert_eq!(
            app.mail_action_enabled(action),
            action == MailAction::Delete,
            "only Delete takes more than one mail: {action:?}"
        );
    }

    // And the bar says which mail or how many: a count is the one thing the
    // seven glyphs cannot show.
    assert_eq!(
        delete_selection_tooltip(1),
        "Delete — removes the selected mail from the server too"
    );
    assert_eq!(
        delete_selection_tooltip(3),
        "Delete — removes the 3 selected mails from the server too"
    );
}

/// One Delete job per folder: a selection made in a folder is one group, and
/// one made over a search's hits is a group for every folder they came from,
/// each in uid order so the same selection is always the same jobs.
#[test]
fn the_delete_groups_the_selection_by_folder() {
    let mails: HashSet<MailKey> = [
        MailKey::new("work", "INBOX", "9"),
        MailKey::new("work", "INBOX", "2"),
        MailKey::new("work", "Sent", "5"),
        MailKey::new("home", "INBOX", "1"),
    ]
    .into_iter()
    .collect();

    let batches = delete_batches(&mails);
    assert_eq!(batches.len(), 3, "one job per folder: {batches:?}");
    assert_eq!(
        batches[&("work".to_string(), "INBOX".to_string())],
        vec!["2", "9"]
    );
    assert_eq!(
        batches[&("work".to_string(), "Sent".to_string())],
        vec!["5"]
    );
    assert_eq!(
        batches[&("home".to_string(), "INBOX".to_string())],
        vec!["1"]
    );
}

/// A mail the server agreed to delete leaves the selection with its row; the
/// mode stays, because the next mail is picked from the same list.
#[test]
fn a_deleted_mail_leaves_the_selection() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 2);
    app.select_mode = true;
    app.selected_mails
        .insert(MailKey::new("acct", "INBOX", "0"));

    app.apply_event(Event::MessageDeleted {
        account_id: "acct".to_string(),
        folder: "INBOX".to_string(),
        uid: "0".to_string(),
    });

    assert!(
        app.selected_mails.is_empty(),
        "the checked mail went with the row"
    );
    assert!(app.select_mode, "select mode is not what a deletion ends");
}

/// The column moves to another list — another folder, another query — and a
/// selection is a selection *of* one list: the checks go with it rather than
/// staying behind for rows that are not on screen any more.
#[test]
fn another_folder_or_query_leaves_select_mode() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_folders(&ctx, &[("INBOX", true, 1), ("Archive", true, 1)]);
    app.select_mode = true;
    app.selected_mails
        .insert(MailKey::new("acct", "INBOX", "INBOX-0"));

    app.select_folder(
        "acct".to_string(),
        "Archive".to_string(),
        crate::models::Protocol::Imap,
    );
    assert!(!app.select_mode, "the column shows another folder now");
    assert!(app.selected_mails.is_empty());

    // And the search popup's Clear: the column is the folder again, which is
    // another list than the hits were.
    app.select_mode = true;
    app.selected_mails
        .insert(MailKey::new("acct", "INBOX", "INBOX-0"));
    app.search.text = String::new();
    app.search_applied = None;
    app.pump_search(true);
    assert!(!app.select_mode, "the column shows the folder again");
    assert!(app.selected_mails.is_empty());
}

/// The select-all box checks every mail the list holds, not only the rows a
/// viewport shows: three mails, one click, three checks — and the mode is
/// still on afterwards.
#[test]
fn the_select_all_box_checks_every_mail_in_the_list() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

    // Into the mode through a row's box, then the band's.
    let at = checkbox_center(&app, 1, false);
    click_at(&ctx, &mut app, at);
    let at = select_all_center(&app);
    click_at(&ctx, &mut app, at);

    assert_eq!(app.selected_mails.len(), 3, "every mail of the folder");
    for uid in ["0", "1", "2"] {
        assert!(
            app.selected_mails
                .contains(&MailKey::new("acct", "INBOX", uid)),
            "mail {uid} is checked"
        );
    }
    assert!(app.select_mode, "the band is not the way out of the mode");
    assert!(
        app.mail_action_enabled(MailAction::Delete),
        "a select-all is exactly what Delete is for"
    );
    assert_eq!(
        app.folder_checkbox(&["0", "1", "2"]),
        Some(CheckState::On),
        "every row's box says so too"
    );
}

/// A second click takes a full selection out again — the label has said
/// Clear selection — and the mode stays where it is, the way unchecking the
/// last mail by hand does.
#[test]
fn the_select_all_box_clears_the_whole_selection() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    app.select_mode = true;
    for uid in ["0", "1", "2"] {
        app.selected_mails
            .insert(MailKey::new("acct", "INBOX", uid));
    }
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

    let at = select_all_center(&app);
    click_at(&ctx, &mut app, at);
    assert!(app.selected_mails.is_empty(), "nothing is checked any more");
    assert!(app.select_mode, "clearing is not leaving");
    assert!(!app.mail_action_enabled(MailAction::Delete));

    // The band's box is an outline again, and no box is filled.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let band = select_all_box_in(&shapes, app.list_column).expect("the band's box");
    assert!(
        strokes_in(&shapes, band).is_empty(),
        "an empty box has no mark"
    );
    assert!(
        fills_in_color(&shapes, theme::of(&ctx).accent).is_empty(),
        "and nothing in the list is filled"
    );
}

/// The band says what is in and what a click would do: an outline with
/// Select all while nothing is checked, the dash while some of it is, and
/// the check with Clear selection once it all is.
#[test]
fn the_select_all_box_shows_off_partial_and_on() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    app.select_mode = true;

    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let band = select_all_box_in(&shapes, app.list_column).expect("the band's box");
    assert!(
        strokes_in(&shapes, band).is_empty(),
        "nothing checked, no mark"
    );
    assert!(fills_in_color(&shapes, theme::of(&ctx).accent).is_empty());
    assert!(frame_texts(&ctx, &mut app)
        .iter()
        .any(|text| text == "Select all"));

    app.selected_mails
        .insert(MailKey::new("acct", "INBOX", "1"));
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let band = select_all_box_in(&shapes, app.list_column).expect("the band's box");
    assert_eq!(
        strokes_in(&shapes, band),
        vec![2],
        "one mail of three is in, so the band draws the dash"
    );

    for uid in ["0", "2"] {
        app.selected_mails
            .insert(MailKey::new("acct", "INBOX", uid));
    }
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let band = select_all_box_in(&shapes, app.list_column).expect("the band's box");
    assert_eq!(
        strokes_in(&shapes, band),
        vec![3],
        "all of them in, so the band draws the check"
    );
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts.iter().any(|text| text == "Clear selection"),
        "the label turned into the way out: {texts:?}"
    );
    assert!(
        !texts.iter().any(|text| text == "Select all"),
        "and it no longer offers to select what is already in: {texts:?}"
    );
}

/// A conversation's row stands for every mail it holds, and the list draws
/// one row for a closed one — but the select-all box is about the list, not
/// about the rows, so both mails of the thread are checked by it.
#[test]
fn select_all_reaches_mail_inside_a_closed_conversation() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_one_conversation(&ctx);
    app.select_mode = true;
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert_eq!(
        app.list_rows.as_ref().expect("rows").rows.len(),
        1,
        "the conversation is one row while it is closed"
    );

    let at = select_all_center(&app);
    click_at(&ctx, &mut app, at);
    assert_eq!(app.selected_mails.len(), 2, "both mails of the thread");
    for uid in ["3", "1"] {
        assert!(
            app.selected_mails
                .contains(&MailKey::new("acct", "INBOX", uid)),
            "mail {uid} is checked"
        );
    }
}

/// The band is the mode's own row, not the list header the column
/// deliberately has none of: view mode draws no box and no label for it,
/// and select mode draws exactly one more box than the rows have.
#[test]
fn the_select_all_row_is_only_drawn_in_select_mode() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let column = app.list_column;
    assert!(
        select_all_box_in(&shapes, column).is_none(),
        "view mode has no select-all row"
    );
    assert_eq!(
        avatars_in(&shapes, column).len(),
        3,
        "and the avatars stand"
    );
    assert!(!frame_texts(&ctx, &mut app)
        .iter()
        .any(|text| text == "Select all"));

    app.select_mode = true;
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(
        select_all_box_in(&shapes, column).is_some(),
        "the band's box"
    );
    assert_eq!(row_boxes_in(&shapes, column).len(), 3, "one per row, still");
    assert!(
        avatars_in(&shapes, column).is_empty(),
        "and the avatars are gone with the mode"
    );
}

/// The band's box is the rows' box: the same column, one line above the
/// first mail — which is what makes it read as the header of that column
/// rather than as a control beside it.
#[test]
fn the_select_all_box_lines_up_with_the_row_boxes() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 2);
    app.select_mode = true;

    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let column = app.list_column;
    let band = select_all_box_in(&shapes, column).expect("the band's box");
    let rows = row_boxes_in(&shapes, column);
    assert_eq!(rows.len(), 2, "one box per mail: {rows:?}");
    assert_eq!(band.center().x, rows[0].center().x, "one column of boxes");
    assert_eq!(
        band.center(),
        select_all_center(&app),
        "the band's own slot"
    );
}

/// Checking everything and then unchecking one mail by hand leaves the band
/// saying *some* of the list is in — the same dash a half-checked
/// conversation gets.
#[test]
fn unchecking_one_mail_leaves_the_select_all_box_partial() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    app.select_mode = true;
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

    let at = select_all_center(&app);
    click_at(&ctx, &mut app, at);
    assert_eq!(app.selected_mails.len(), 3);

    let at = checkbox_center(&app, 1, false);
    click_at(&ctx, &mut app, at);
    assert_eq!(app.selected_mails.len(), 2, "one mail was taken out");
    assert_eq!(
        app.folder_checkbox(&["0", "1", "2"]),
        Some(CheckState::Partial),
        "the row's box still says so, and"
    );
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let band = select_all_box_in(&shapes, app.list_column).expect("the band's box");
    assert_eq!(
        strokes_in(&shapes, band),
        vec![2],
        "the band draws the dash for the rest"
    );
}

/// What the band's box says about the selection and the list, on its own:
/// nothing in is off, everything in is on, and the size comparison is the
/// whole of it.
#[test]
fn the_select_all_state_compares_the_selection_with_the_list() {
    assert_eq!(select_all_state(0, 0), CheckState::Off, "an empty list");
    assert_eq!(select_all_state(0, 3), CheckState::Off);
    assert_eq!(select_all_state(1, 3), CheckState::Partial);
    assert_eq!(select_all_state(3, 3), CheckState::On);
}

/// The label is the action a click would take, and the count — the one
/// thing a one-line row cannot show — is in the hover text.
#[test]
fn the_select_all_tooltip_counts_the_mails() {
    assert_eq!(select_all_label(CheckState::Off), "Select all");
    assert_eq!(select_all_label(CheckState::Partial), "Select all");
    assert_eq!(select_all_label(CheckState::On), "Clear selection");

    assert_eq!(
        select_all_tooltip(CheckState::Off, 1),
        "Select the mail in this list"
    );
    assert_eq!(
        select_all_tooltip(CheckState::Partial, 3),
        "Select all 3 mails in this list"
    );
    assert_eq!(
        select_all_tooltip(CheckState::On, 1),
        "Clear the selection — the mail in this list is checked"
    );
    assert_eq!(
        select_all_tooltip(CheckState::On, 3),
        "Clear the selection — all 3 mails in this list are checked"
    );
}

/// A UIDVALIDITY rotation clears the folder's mail because the UIDs are
/// meaningless; a check made of one is meaningless with it, and the band
/// counts the selection against the list.
#[test]
fn a_rotated_uidvalidity_drops_its_folder_from_the_selection() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.select_mode = true;
    app.selected_mails
        .insert(MailKey::new("acct", "INBOX", "0"));

    app.apply_event(Event::Messages {
        account_id: "acct".to_string(),
        folder: "INBOX".to_string(),
        // The fixture's folder is generation 1: this is a new one.
        uidvalidity: Some(2),
        messages: Vec::new(),
        // A rotation refetches the folder whole; nothing about it is news.
        new_mail: false,
    });

    assert!(
        app.states["acct"].mailbox.folders[0].messages.is_empty(),
        "the folder was cleared"
    );
    assert!(
        app.selected_mails.is_empty(),
        "and the checks of the old UIDs went with it"
    );
}

/// The select-all row is the one thing in the column that does not scroll
/// away: however far down the list is, the box is still under the title bar —
/// and still the whole list's, not the viewport's.
#[test]
fn the_select_all_row_stays_while_the_list_scrolls() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 60);
    app.select_mode = true;
    run_app_frame(&ctx, &mut app, Vec::new());

    // Down the list: the pointer over it, the wheel, then the frames the
    // scroll area's animation needs to come to rest.
    run_app_frame(
        &ctx,
        &mut app,
        vec![egui::Event::PointerMoved(egui::pos2(300.0, 400.0))],
    );
    run_app_frame(
        &ctx,
        &mut app,
        vec![egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -600.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::default(),
        }],
    );
    for _ in 0..30 {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    assert!(app.list_scroll > 0.0, "the list did not scroll");

    // The band is where it was: pinned under the title bar, over the rows.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let band = select_all_box_in(&shapes, app.list_column).expect("the band's box");
    assert_eq!(
        band.center(),
        select_all_center(&app),
        "the band scrolled away with the list"
    );

    // And it is still the control it was: a click checks the whole list.
    let at = select_all_center(&app);
    click_at(&ctx, &mut app, at);
    assert_eq!(
        app.selected_mails.len(),
        60,
        "the band checks the folder, not the rows a viewport shows"
    );
}

/// In select mode the row's own button is not the selection's: it takes the
/// one mail the row speaks for and leaves the mode and the checks alone.
#[test]
fn the_row_delete_button_leaves_select_mode_alone() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    app.select_mode = true;
    app.selected_mails
        .insert(MailKey::new("acct", "INBOX", "0"));
    let _ = frame_shapes_at(&ctx, &mut app, 1400.0);

    let at = row_delete_center(&app, 2);
    click_at(&ctx, &mut app, at);
    assert!(app.select_mode, "the row's button is not the way out");
    assert_eq!(
        app.selected_mails.len(),
        1,
        "the checks are the box's business"
    );
    assert!(
        app.selected_mails
            .contains(&MailKey::new("acct", "INBOX", "0")),
        "and the checked mail is still checked"
    );
    assert_eq!(app.selected_message, None, "and no mail was opened");
}
