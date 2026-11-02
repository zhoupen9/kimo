use super::super::*;
use super::common::*;

/// Gmail's LIST names a '[Gmail]' container that SELECT refuses, nests
/// its system mailboxes under it, and lists user labels beside INBOX. The
/// column drops the container, shows '[Gmail]/All Mail' as "All Mail",
/// gives every label the same tag glyph, and orders INBOX, the system
/// folders, then the labels.
#[test]
fn gmail_folders_lose_their_prefix_and_labels_come_last() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_folders(
        &ctx,
        &[
            ("INBOX", true, 1),
            ("[Gmail]", true, 0),
            ("[Gmail]/All Mail", true, 0),
            ("[Gmail]/Drafts", true, 0),
            ("[Gmail]/Sent Mail", true, 0),
            ("[Gmail]/Spam", true, 0),
            ("[Gmail]/Starred", true, 0),
            ("[Gmail]/Trash", true, 0),
            ("Receipts", true, 0),
            ("Projects/2026", true, 0),
            ("Sent", true, 0),
        ],
    );
    {
        let nav = app.states.get_mut("acct").unwrap().nav();
        assert_eq!(
            names_of(&nav.listed),
            [
                "INBOX",
                "[Gmail]/All Mail",
                "[Gmail]/Drafts",
                "[Gmail]/Sent Mail",
                "[Gmail]/Spam",
                "[Gmail]/Starred",
                "[Gmail]/Trash",
                "Projects/2026",
                "Receipts",
                "Sent",
            ]
        );
        let displays: Vec<&str> = nav.listed.iter().map(|row| row.display.as_str()).collect();
        assert_eq!(
            displays,
            [
                "INBOX",
                "All Mail",
                "Drafts",
                "Sent Mail",
                "Spam",
                "Starred",
                "Trash",
                "Projects/2026",
                "Receipts",
                "Sent",
            ]
        );
        // The system folders keep the icons their stripped names classify;
        // a label wears the tag, even one named like a folder.
        assert!(!nav.listed[0].label && !nav.listed[1].label);
        assert_eq!(nav.listed[1].kind, FolderKind::AllMail);
        assert_eq!(nav.listed[2].kind, FolderKind::Drafts);
        assert_eq!(nav.listed[6].kind, FolderKind::Trash);
        for index in 7..10 {
            assert!(
                nav.listed[index].label,
                "{} is a label",
                nav.listed[index].name
            );
        }
        assert_eq!(
            nav.listed[9].kind,
            FolderKind::Sent,
            "a label can be named Sent"
        );
    }

    // What the column draws, in the order it draws it: the prefix is
    // display decoration, and the container is not a row at all.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let nav = nav_column(&app);
    let mut tops = Vec::new();
    for shown in [
        "INBOX",
        "All Mail",
        "Drafts",
        "Sent Mail",
        "Spam",
        "Starred",
        "Trash",
        "Projects/2026",
        "Receipts",
        "Sent",
    ] {
        let row = text_boxes_of(&shapes, nav, shown)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("{shown} is not drawn: {shapes:?}"));
        tops.push((shown, row.top()));
    }
    assert!(
        tops.windows(2).all(|pair| pair[0].1 < pair[1].1),
        "INBOX, then Gmail's folders, then the labels: {tops:?}"
    );
    assert!(
        text_boxes_of(&shapes, nav, "[Gmail]").is_empty(),
        "the container is not a mailbox"
    );
}

/// The labels are a group of their own, and the navigation column's last
/// one: under every account and its folders, headed by the word alone — a
/// label is not an account, so there is nothing to collapse — with the mail
/// each holds counted beside it.
#[test]
fn the_labels_are_the_navigation_columns_last_group() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_two_accounts(&ctx, 2);
    label_mail(&mut app, "acct", "INBOX", "acct-0", "Work");
    label_mail(&mut app, "acct", "INBOX", "acct-1", "Work");
    label_mail(&mut app, "other", "INBOX", "other-0", "Travel");

    // What the group says, before the frame draws it: both labels, in name
    // order, each with the amount of mail under it.
    app.refresh_label_view();
    let rows = &app.label_view.as_ref().expect("derived just above").rows;
    assert_eq!(
        rows.iter()
            .map(|row| (row.name.as_str(), row.count, row.selected))
            .collect::<Vec<_>>(),
        [("Travel", 1, false), ("Work", 2, false)]
    );

    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let nav = nav_column(&app);
    let heading = text_boxes_of(&shapes, nav, "Labels")
        .into_iter()
        .next()
        .expect("the group is headed");
    let work = text_boxes_of(&shapes, nav, "Work")
        .into_iter()
        .next()
        .expect("the row is drawn");
    let travel = text_boxes_of(&shapes, nav, "Travel")
        .into_iter()
        .next()
        .expect("the second row too");
    // The heading, then the rows in name order: Travel before Work.
    assert!(
        heading.top() < travel.top() && travel.bottom() <= work.top(),
        "the group draws its heading and then its rows: {heading:?} {travel:?} {work:?}"
    );
    // Under every account: the folders of both are above the heading, and
    // an account's name with them.
    for folder in ["INBOX", "acct", "other"] {
        for row in text_boxes_of(&shapes, nav, folder) {
            assert!(
                row.bottom() <= heading.top(),
                "{folder} is drawn under the label group: {row:?} against {heading:?}"
            );
        }
    }
}

/// A click on a label is a filter over the whole mailbox: the list becomes
/// the mail carrying it — the mail of every folder and account at once, each
/// row saying where it lives — and a click on the same label takes the
/// filter back out.
#[test]
fn clicking_a_label_narrows_the_list_to_its_mail() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_two_accounts(&ctx, 2);
    // The two accounts' mail, one labelled each: the folder's own list is
    // never what the filter shows.
    label_mail(&mut app, "acct", "INBOX", "acct-0", "Work");
    label_mail(&mut app, "other", "INBOX", "other-1", "Work");

    click_label(&ctx, &mut app, "Work");
    assert_eq!(app.label_filter, ["Work".to_string()]);

    let texts = frame_texts(&ctx, &mut app);
    let drawn = |text: &str| texts.iter().any(|run| run == text);
    assert!(drawn("Subject acct-0") && drawn("Subject other-1"));
    assert!(
        !drawn("Subject acct-1") && !drawn("Subject other-0"),
        "the list is the label's mail, not the folder's: {texts:?}"
    );
    // The rows are the search rows' shape: where a mail lives, because the
    // filter spans the accounts.
    assert!(drawn("acct · INBOX") && drawn("other · INBOX"), "{texts:?}");

    // The same click again: the filter is off, and the column is the
    // folder's list.
    click_label(&ctx, &mut app, "Work");
    assert!(app.label_filter.is_empty());
    let texts = frame_texts(&ctx, &mut app);
    assert!(texts.iter().any(|run| run == "Subject acct-1"));
    assert!(!texts.iter().any(|run| run == "Subject other-1"));
}

/// Two labels narrow the list rather than widening it: the filter is an AND,
/// so the list is the mail that carries both — and taking one back out
/// leaves the other's mail.
#[test]
fn two_labels_filter_with_an_and_and_a_click_takes_one_back() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_two_accounts(&ctx, 2);
    label_mail(&mut app, "acct", "INBOX", "acct-0", "Work");
    label_mail(&mut app, "acct", "INBOX", "acct-1", "Work");
    label_mail(&mut app, "acct", "INBOX", "acct-1", "Travel");
    label_mail(&mut app, "other", "INBOX", "other-0", "Travel");

    click_label(&ctx, &mut app, "Work");
    let texts = frame_texts(&ctx, &mut app);
    let drawn = |text: &str| texts.iter().any(|run| run == text);
    assert!(drawn("Subject acct-0") && drawn("Subject acct-1"));
    assert!(!drawn("Subject other-0"));

    click_label(&ctx, &mut app, "Travel");
    assert_eq!(app.label_filter, ["Travel".to_string(), "Work".to_string()]);
    let texts = frame_texts(&ctx, &mut app);
    let drawn = |text: &str| texts.iter().any(|run| run == text);
    assert!(
        drawn("Subject acct-1"),
        "the one mail carrying both: {texts:?}"
    );
    assert!(
        !drawn("Subject acct-0") && !drawn("Subject other-0"),
        "an AND narrows: {texts:?}"
    );

    // One out again: the other's mail comes back.
    click_label(&ctx, &mut app, "Work");
    assert_eq!(app.label_filter, ["Travel".to_string()]);
    let texts = frame_texts(&ctx, &mut app);
    let drawn = |text: &str| texts.iter().any(|run| run == text);
    assert!(drawn("Subject acct-1") && drawn("Subject other-0"));
    assert!(!drawn("Subject acct-0"));
}

/// A query and a label filter are two answers to what the middle column
/// shows, and the label is the one that was just asked for: looking at a
/// label gives an active search up rather than stacking the two into a list
/// that is neither.
#[test]
fn a_label_filter_gives_up_an_active_search() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_two_accounts(&ctx, 2);
    label_mail(&mut app, "acct", "INBOX", "acct-0", "Work");
    app.search.text = "acct".to_string();
    assert!(app.search.is_active());

    click_label(&ctx, &mut app, "Work");
    assert_eq!(app.label_filter, ["Work".to_string()]);
    assert!(!app.search.is_active() && app.search.text.is_empty());
    assert!(
        app.search_applied
            .as_ref()
            .is_some_and(|query| !query.is_active()),
        "the applied query is the emptied one, so nothing is searched for it"
    );
}

/// The label cell on a mail's own head opens the popup about that mail, the
/// way its reply cell opens the compose: the labels it carries are ticked,
/// a click in the list puts one on it or takes it off, and the card says so
/// afterwards.
#[test]
fn the_label_cell_labels_the_mail_it_is_on() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 2);
    app.labels = vec!["Travel".to_string(), "Work".to_string()];
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());

    click_at(&ctx, &mut app, header_action_center(1));
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts.iter().any(|run| run == "New label…"),
        "the popup is the label list and a box: {texts:?}"
    );
    // The checklist is the app's own labels, over the columns rather than in
    // them.
    let boxes = |text: &str| text_boxes_of(&shapes, popup_area(), text);
    assert_eq!(boxes("Work").len(), 1, "the popup lists the label once");
    assert_eq!(boxes("Travel").len(), 1);

    // A click on a label in the popup puts it on this mail.
    let work = boxes("Work")[0].center();
    click_at(&ctx, &mut app, work);
    let key = MailKey::new("acct", "INBOX", "0");
    assert_eq!(app.mail_labels(&key), ["Work".to_string()]);
    // The card says it: the mail carries a chip of its own now.
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts.iter().any(|run| run == "Work"),
        "the card shows the label: {texts:?}"
    );

    // And a click on it again takes it off — the popup stayed open, and the
    // mail's chip is a different thing at another place: the popup is what
    // is drawn over the columns.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let work = text_boxes_of(&shapes, popup_area(), "Work")
        .pop()
        .expect("the popup is still open");
    click_at(&ctx, &mut app, work.center());
    assert!(app.mail_labels(&key).is_empty());
}

/// A name the app does not have is made a label by being put on a mail: the
/// popup's box is the only place one comes into being, and the navigation
/// column's group has it from that moment on.
#[test]
fn a_new_label_is_made_for_the_mail_in_the_popup() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 2);
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());
    assert!(app.labels.is_empty());

    // The label cell on the mail's head, and the popup it opens: with no
    // labels at all it says so, and offers the box.
    click_at(&ctx, &mut app, header_action_center(1));
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts
            .iter()
            .any(|run| run == "No labels yet — name one below."),
        "{texts:?}"
    );

    // Type a name in the box and press the button.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let field = text_boxes_of(&shapes, popup_area(), "New label…")
        .pop()
        .expect("the box is in the popup");
    click_at(&ctx, &mut app, field.center());
    run_app_frame(
        &ctx,
        &mut app,
        vec![egui::Event::Text("Receipts".to_string())],
    );
    assert_eq!(app.label_draft, "Receipts", "the box holds what was typed");

    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let create = text_boxes_of(&shapes, popup_area(), "Create")
        .pop()
        .expect("the button is in the popup");
    click_at(&ctx, &mut app, create.center());

    assert_eq!(app.labels, ["Receipts".to_string()], "the label was made");
    let key = MailKey::new("acct", "INBOX", "0");
    assert_eq!(app.mail_labels(&key), ["Receipts".to_string()]);
    assert!(
        app.label_draft.is_empty(),
        "the box is emptied by the click"
    );
    // And the column draws it from here on.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(
        !text_boxes_of(&shapes, nav_column(&app), "Receipts").is_empty(),
        "the group has the new label"
    );
}

/// The title bar's own label cell opens the same popup, about the mail the
/// reading pane shows: the bar answers the pane's mail, the card head the
/// mail it sits on.
#[test]
fn the_title_bars_label_cell_opens_the_popup_too() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());

    let centre = titlebar_action_center(&ctx, &mut app, MailAction::Label, 1400.0);
    // The bar drew the cell where the arithmetic says (the fill under the
    // pointer is the cell's own), and the click opens the popup.
    assert!(
        fill_under_the_pointer(&ctx, &mut app, 1400.0, centre, TITLEBAR_BUTTON_SIZE).is_some(),
        "no label cell at {centre:?}"
    );
    click_at(&ctx, &mut app, centre);
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts
            .iter()
            .any(|run| run == "No labels yet — name one below."),
        "the bar's label cell opened the popup: {texts:?}"
    );
}

/// A mail's card says what it is labelled with, under its head: one chip per
/// label, as wide as the name wants. A mail nobody labelled carries no row at
/// all, so the head is still the first thing on it.
#[test]
fn a_mails_card_says_what_it_is_labelled_with() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 2);
    app.selected_message = Some("0".to_string());
    let pane = |app: &KimoApp| {
        egui::Rect::from_min_max(
            egui::pos2(app.list_column.right(), 0.0),
            egui::pos2(1400.0, 900.0),
        )
    };

    // Nothing labelled: no chips, and the subject follows the head row.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(text_boxes_of(&shapes, pane(&app), "Work").is_empty());

    label_mail(&mut app, "acct", "INBOX", "0", "Work");
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let name = text_boxes_of(&shapes, pane(&app), "Work")
        .pop()
        .expect("the chip's name is on the card");
    let head_bottom = TITLEBAR_HEIGHT
        + CONTENT_GUTTER
        + CONTENT_PADDING.y
        + AVATAR_DIAMETER.max(CARD_ACTION_SIZE);
    assert!(
        name.top() >= head_bottom,
        "the chips go under the head row: {name:?} against {head_bottom}"
    );
    let scheme = theme::of(&ctx);
    let chip = fills_in_color(&shapes, scheme.hover)
        .into_iter()
        .find(|rect| rect.contains_rect(name))
        .unwrap_or_else(|| panic!("nothing filled under {name:?}"));
    assert!((chip.height() - LABEL_CHIP_HEIGHT).abs() < 0.5, "{chip:?}");
    assert!(
        (chip.width() - (name.width() + LABEL_CHIP_PADDING * 2.0)).abs() < 0.5,
        "the chip is the name and its padding: {chip:?}"
    );
}
