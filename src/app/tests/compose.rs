use super::super::*;
use super::common::*;

#[test]
fn a_new_draft_is_written_from_the_open_account_or_the_first() {
    let mut personal = test_account("imap.example.com");
    personal.id = "personal".to_string();
    let accounts = vec![test_account("imap.example.com"), personal];

    // The account whose mail is open, while it is still configured.
    assert_eq!(
        compose_from(&accounts, Some("personal")).as_deref(),
        Some("personal")
    );
    // Nothing open — or an account that has since been removed — falls back
    // to the first configured one rather than to no sender at all.
    assert_eq!(compose_from(&accounts, None).as_deref(), Some("work"));
    assert_eq!(
        compose_from(&accounts, Some("deleted")).as_deref(),
        Some("work")
    );
    // With nothing configured there is no sender to pick, which is the one
    // case the window says so.
    assert_eq!(compose_from(&[], Some("work")), None);
}

/// A click on the button asks the server, and the answer is what the button
/// is drawn from: the event a job sends when it finds nothing below the
/// floor takes it away again. (The job itself needs a server; this is the
/// half of it that is the app's.)
#[test]
fn the_answer_about_older_mail_is_what_draws_the_button() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);

    app.apply_event(Event::OlderMail {
        account_id: "acct".to_string(),
        folder: "INBOX".to_string(),
        available: true,
    });
    assert!(frame_texts(&ctx, &mut app)
        .iter()
        .any(|text| text == "Load older mail"));

    app.apply_event(Event::OlderMail {
        account_id: "acct".to_string(),
        folder: "INBOX".to_string(),
        available: false,
    });
    assert!(!frame_texts(&ctx, &mut app)
        .iter()
        .any(|text| text == "Load older mail"));
}

#[test]
fn sender_initial_prefers_name_over_email() {
    assert_eq!(sender_initial("Alice <alice@example.com>"), "A");
    assert_eq!(sender_initial("张三 <zhang@nroad.com.cn>"), "张");
    assert_eq!(sender_initial("bob@example.com"), "B");
    assert_eq!(sender_initial(""), "?");
}

#[test]
fn sender_color_is_deterministic() {
    assert_eq!(
        sender_color(theme::DARK, "a@b.c"),
        sender_color(theme::LIGHT, "a@b.c")
    );
    assert_ne!(
        sender_color(theme::DARK, "a@b.c"),
        sender_color(theme::DARK, "other@b.c")
    );
}

/// The draft is the only copy of the mail, so the server saying it has the
/// mail is what closes the card — and a refusal keeps it, with the server's
/// own words in it, ready to be sent again.
#[test]
fn a_sent_mail_closes_the_card_and_a_refusal_keeps_it() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.compose = Some(ComposeDraft::new(Some("acct".to_string())));
    if let Some(draft) = app.compose.as_mut() {
        draft.to = "bob@example.com".to_string();
        draft.subject = "Hello".to_string();
        draft.body = "Body".to_string();
        draft.sending = true;
    }
    // A send in flight: the card says so and its boxes are disabled. The
    // first frame is what sizes a modal's area — the card is drawn from the
    // second one on, which is what the app does live too.
    let _ = frame_texts(&ctx, &mut app);
    let texts = frame_texts(&ctx, &mut app);
    assert!(texts.iter().any(|text| text == "Sending…"), "{texts:?}");

    // The server refused it: the draft stays, the reason is in the card,
    // and Send is enabled again.
    app.apply_event(Event::MessageSendFailed {
        error: "550 5.1.1 no such user".to_string(),
    });
    let draft = app.compose.as_ref().expect("the draft is still there");
    assert!(!draft.sending);
    assert_eq!(draft.error.as_deref(), Some("550 5.1.1 no such user"));
    assert_eq!(draft.to, "bob@example.com", "the draft is untouched");
    let _ = frame_texts(&ctx, &mut app);
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts.iter().any(|text| text.contains("no such user")),
        "{texts:?}"
    );

    // The server has it now: the card goes with the draft, and the line
    // above the message list says where the mail went.
    app.apply_event(Event::MessageSent {
        recipients: vec!["bob@example.com".to_string()],
        copy: SentCopy::Saved {
            mailbox: "Sent".to_string(),
        },
    });
    assert!(app.compose.is_none(), "a sent card is a closed card");
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts
            .iter()
            .any(|text| text == "Sent to bob@example.com · a copy is in Sent"),
        "{texts:?}"
    );
}

/// The three composes that answer a mail are made of it: a reply is
/// addressed to its sender alone, a reply all to everyone but the user's
/// own address, a forward to nobody until the sender chooses — and all
/// three carry the original quoted under the new text, with the subject
/// prefixed unless it already carries a prefix of its own.
#[test]
fn the_answers_to_a_mail_are_made_of_it() {
    let mail = mail_to_answer();
    let quote = "On Tue, 2026-01-01, Ada <ada@example.com> wrote:\n> Here it is.\n> Second line.\n";

    let reply = ComposeDraft::reply(&mail, Some("acct".to_string()));
    assert_eq!(reply.kind, ComposeKind::Reply);
    assert_eq!(reply.to, "Ada <ada@example.com>");
    assert_eq!(reply.cc, "");
    assert_eq!(reply.subject, "Re: The report");
    assert_eq!(reply.body, quote);

    let all = ComposeDraft::reply_all(&mail, Some("acct".to_string()), "me@example.com");
    assert_eq!(all.kind, ComposeKind::ReplyAll);
    assert_eq!(all.to, "Ada <ada@example.com>");
    assert_eq!(
        all.cc, "bob@example.com",
        "the user's own address is not sent their own mail"
    );
    assert_eq!(all.subject, "Re: The report");

    let forward = ComposeDraft::forward(&mail, Some("acct".to_string()));
    assert_eq!(forward.kind, ComposeKind::Forward);
    assert_eq!(forward.to, "");
    assert_eq!(forward.cc, "");
    assert_eq!(forward.subject, "Fwd: The report");
    assert_eq!(forward.body, quote);

    // A subject that already answers is not prefixed again — whatever
    // prefix the sender's client wrote.
    assert_eq!(answer_subject("Re", "Re: The report"), "Re: The report");
    assert_eq!(answer_subject("Re", "答复: 报告"), "答复: 报告");
    assert_eq!(answer_subject("Fwd", "The report"), "Fwd: The report");
}

/// A mail's own header answers it where the mail is: the reply cell opens
/// the compose in the pane itself, prefilled from the mail — a card right
/// under it, so the pane keeps the mail on screen and says both things at
/// once. The title bar's cell opens the same draft as a popup over
/// everything.
#[test]
fn a_mails_header_answers_it_where_the_mail_is() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());

    // The reply cell, on the mail's header: the card comes up under the
    // mail.
    click_at(&ctx, &mut app, header_action_center(4));
    let draft = app.compose.as_ref().expect("the reply opened a draft");
    assert_eq!(app.compose_spot, ComposeSpot::Pane);
    assert_eq!(draft.kind, ComposeKind::Reply);
    assert_eq!(draft.to, "Ada <ada@example.com>");
    assert_eq!(draft.subject, "Re: Subject 0");
    assert_eq!(
        draft.body,
        "On 2026-01-01, Ada <ada@example.com> wrote:\n> body\n"
    );

    // The pane says both things at once: the mail above, and the card under
    // it — which, being under its own mail, does not say what it is: its
    // first line is who the answer is from, in the place the heading took,
    // and Cc, Bcc and Subject are not among the fields it shows.
    let texts = frame_texts(&ctx, &mut app);
    assert!(texts.iter().any(|text| text == "Subject 0"));
    assert!(
        texts.iter().any(|text| text == "Test <ada>"),
        "the account the answer goes from is on its head bar: {texts:?}"
    );
    assert!(
        !texts.iter().any(|text| text == "Reply") && texts.iter().any(|text| text == "From:"),
        "the place says what this card is, and its From row is labelled: {texts:?}"
    );
    for absent in ["Cc:", "Bcc:", "Subject"] {
        assert!(
            !texts.iter().any(|text| text == absent),
            "an answer written under its mail shows the head bar, To, the files, \
                 the body and the two buttons: {absent} is not one of them, {texts:?}"
        );
    }

    // The head bar's label is the account's own face: a filled circle in the
    // colour its address hashes to, in the label column's place on the card
    // that answers the mail.
    let colour = sender_color(theme::of(&ctx), "ada");
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let answer = *pane_cards(&ctx, &mut app)
        .last()
        .expect("the answer is a card of the pane");
    let avatars: Vec<egui::Pos2> = circles_in_color(&shapes, colour)
        .into_iter()
        .filter(|centre| answer.contains(*centre))
        .collect();
    assert_eq!(
        avatars.len(),
        1,
        "one face on the answer's head bar: {avatars:?} of {answer:?}"
    );

    // The reply's own To is flat beside that face, like the From list
    // above it: the card is a letter, not the popup's form, so the row
    // wears no box — no fill and no outline. The body's own field is the
    // only surface the answer's fields leave on the card, which is what
    // the scheme's input fill counts.
    let fields: Vec<egui::Rect> = fills_in_color(&shapes, theme::of(&ctx).input)
        .into_iter()
        .filter(|rect| answer.contains(rect.center()))
        .collect();
    assert_eq!(
        fields.len(),
        1,
        "only the body is drawn as a box: {fields:?} of {answer:?}"
    );

    // And the footer's two are the card's own: they stand a whole button
    // tall — the stock ones are a line of a form — and one of them, Send,
    // has the plane drawn inside it. (Send is the *disabled* one here: the
    // fixture account has no outgoing server, and a disabled button is the
    // card's own fill rather than the accent.)
    let scheme = theme::of(&ctx);
    let buttons: Vec<egui::Rect> = fills_in_color(&shapes, scheme.card)
        .into_iter()
        .filter(|rect| {
            (rect.height() - COMPOSE_BUTTON_HEIGHT).abs() < 0.5 && answer.contains(rect.center())
        })
        .collect();
    assert_eq!(buttons.len(), 2, "Send and Discard: {buttons:?}");
    assert!(
        buttons.iter().all(|rect| rect.width() > 60.0),
        "an icon and a word fit in each: {buttons:?}"
    );
    let planes: Vec<&egui::Rect> = buttons
        .iter()
        .filter(|rect| !glyph_colors_in(&shapes, **rect).is_empty())
        .collect();
    assert_eq!(planes.len(), 1, "the plane is on Send alone: {buttons:?}");

    // The title bar's cell opens the same three as popups — the same
    // draft, another container.
    let mail = app.selected_mail().cloned().expect("the mail to answer");
    app.compose_about(MailAction::ReplyAll, ComposeSpot::Popup, &mail);
    assert_eq!(app.compose_spot, ComposeSpot::Popup);
    assert_eq!(
        app.compose.as_ref().expect("draft").kind,
        ComposeKind::ReplyAll
    );
    // And the popup is the whole form: the same draft in the other container
    // has its heading back and Subject, and Cc and Bcc are folded away
    // behind the To row's chevron the way the spot's are — a Reply all's own
    // Cc is one click from being read (`docs/ui-style.md`). The frames are
    // what the modal's fade needs before a fill is its own colour again.
    for _ in 0..FADE_FRAMES {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    let texts = frame_texts(&ctx, &mut app);
    for present in ["Reply all", "Subject:"] {
        assert!(
            texts.iter().any(|text| text == present),
            "{present} is on the popup's form: {texts:?}"
        );
    }
    for folded in ["Cc:", "Bcc:"] {
        assert!(
            !texts.iter().any(|text| text == folded),
            "the popup folds {folded} away until it is asked for: {texts:?}"
        );
    }
    // The chevron at the end of To is what asks: it is the last line drawing
    // in the popup's area — the mail card's own action glyphs are above it —
    // and a click brings the two rows out under the row.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let fold = chevrons_in(&shapes, popup_area())
        .pop()
        .expect("To ends with the fold chevron");
    click_at(&ctx, &mut app, egui::Rect::from_points(&fold).center());
    let texts = frame_texts(&ctx, &mut app);
    for shown in ["Cc:", "Bcc:"] {
        assert!(
            texts.iter().any(|text| text == shown),
            "the chevron brings {shown} out: {texts:?}"
        );
    }
    // And the popup's foot is the card's own pair now, not egui's buttons:
    // two fills a button tall, in the colours `compose_button` draws with —
    // the fixture account has no outgoing server, so both are the card's
    // own disabled fill rather than Send's accent.
    let scheme = theme::of(&ctx);
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let buttons: Vec<egui::Rect> = [scheme.card, scheme.accent]
        .into_iter()
        .flat_map(|fill| fills_in_color(&shapes, fill))
        .filter(|rect| {
            (rect.height() - COMPOSE_BUTTON_HEIGHT).abs() < 0.5
                && popup_area().contains(rect.center())
        })
        .collect();
    assert_eq!(
        buttons.len(),
        2,
        "the popup draws the card's own two buttons: {buttons:?}"
    );
}

/// The answer a mail's header opens is one more card of the pane, right
/// under the mail it answers: the conversation stays whole on screen and
/// the card sits in it — here between the mail answered and the rest of
/// the thread, a card gap from each. And it is the clicked mail's answer,
/// not the conversation's opened mail's: the header the click was on is
/// the mail the draft is made of.
#[test]
fn the_answer_sits_right_under_the_mail_it_answers() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_one_conversation(&ctx);
    app.selected_message = Some("3".to_string());
    // A frame to settle the columns; then the conversation is two cards.
    let _ = pane_cards(&ctx, &mut app);
    assert_eq!(
        pane_cards(&ctx, &mut app).len(),
        2,
        "the conversation is a card per mail"
    );

    // The oldest mail's reply cell — the conversation was opened on the
    // newest, and the answer is the oldest's own.
    click_at(&ctx, &mut app, header_action_center(4));
    let draft = app.compose.as_ref().expect("the reply opened a draft");
    assert_eq!(app.compose_spot, ComposeSpot::Pane);
    assert_eq!(
        app.compose_anchor
            .as_ref()
            .expect("the draft knows its mail")
            .uid,
        "1"
    );
    assert!(
        draft.body.contains("body of 1"),
        "the draft quotes the mail whose header was clicked: {}",
        draft.body
    );

    // The card sits between the two mails, a card gap from each.
    let cards = pane_cards(&ctx, &mut app);
    assert_eq!(
        cards.len(),
        3,
        "the answer is a card of the pane: {cards:?}"
    );
    assert!(
        (cards[1].top() - cards[0].bottom() - CONVERSATION_CARD_GAP).abs() < 0.5,
        "the answer is right under the mail it answers: {cards:?}"
    );
    assert!(
        (cards[2].top() - cards[1].bottom() - CONVERSATION_CARD_GAP).abs() < 0.5,
        "the thread goes on under the answer: {cards:?}"
    );

    // And nothing the pane held was replaced by the answer: the mail above
    // it still says its piece, and the answer's own card is under it. The
    // thread's tail is below the fold by then — bringing the answer into
    // view is what the pane spent its room on (`compose_inline_card`) — so
    // the two mails are counted as cards rather than read off one frame.
    let texts = frame_texts(&ctx, &mut app);
    for expected in [
        "body of 1",
        "Click to select attachments, or drag files here",
    ] {
        assert!(
            texts.iter().any(|text| text == expected),
            "{expected} is on the pane: {texts:?}"
        );
    }
}

/// An answer opened on the spot comes to the reader rather than waiting
/// below the fold, and its keyboard is where the writing goes: the pane
/// scrolls the new card into view on the frame it is first drawn — a reply
/// cell at the top of a long mail is what opens it, and the card under that
/// mail is otherwise off screen — and the caret is put in the body, above
/// the quoted original, because the answer already knows who it goes to.
#[test]
fn an_answer_opened_on_the_spot_is_revealed_and_typed_into_at_its_body() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    // A mail long enough that the card under it opens below the pane.
    let long: String = (0..60).map(|line| format!("line {line}\n")).collect();
    app.states
        .get_mut("acct")
        .expect("the fixture account")
        .mailbox
        .folders[0]
        .messages[0]
        .body_text = long;
    app.selected_message = Some("0".to_string());
    // A frame on the mail itself: its header starts the pane, and nothing
    // has scrolled it.
    run_app_frame(&ctx, &mut app, Vec::new());
    assert_eq!(app.mail_scroll, 0.0, "the pane sits on the mail's head");

    click_at(&ctx, &mut app, header_action_center(4));
    assert_eq!(app.compose_spot, ComposeSpot::Pane);
    assert_eq!(
        app.compose.as_ref().expect("the reply opened a draft").to,
        "Ada <ada@example.com>",
        "a reply arrives addressed, which is what puts the keyboard in the body"
    );
    // The scroll is egui's own, animated (`Ui::scroll_to_rect`), so the pane
    // is given the frames it asks for.
    for _ in 0..40 {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    assert!(
        app.mail_scroll > 0.0,
        "the pane scrolled the answer up: {}",
        app.mail_scroll
    );

    // And it scrolled to the answer: the card is on screen whole — the
    // mail above gave way by no more than the card's own height — where
    // before the click the pane held only the head of a mail that ran on
    // below its floor.
    let answer = *pane_cards(&ctx, &mut app)
        .last()
        .expect("the answer is a card of the pane");
    let window = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1400.0, 900.0));
    assert!(
        window.contains_rect(answer),
        "the answer is on screen: {answer:?}"
    );

    // The keyboard is in the body: what is typed is the reply, and it goes
    // in *above* the quote rather than appended after it.
    run_app_frame(&ctx, &mut app, vec![egui::Event::Text("Hello".to_string())]);
    let draft = app.compose.as_ref().expect("the draft is still open");
    assert!(
        draft.body.starts_with("HelloOn ") && draft.body.contains("> line 0"),
        "the reply starts above the quoted original: {:?}",
        draft.body
    );
    assert_eq!(
        draft.to, "Ada <ada@example.com>",
        "the typing did not land in To"
    );
}

/// A forward is the one answer opened on the spot that has no recipient
/// yet, so its keyboard starts at To — the body's opening caret is for an
/// answer that already knows who it goes to. The flat To takes the keyboard
/// like any field: it is the box that is gone, not the field.
#[test]
fn a_forward_opened_on_the_spot_starts_at_to() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());

    // Two cells in from the right: forward, between label and reply all.
    click_at(&ctx, &mut app, header_action_center(2));
    assert_eq!(app.compose_spot, ComposeSpot::Pane);
    let draft = app.compose.as_ref().expect("the forward opened a draft");
    assert_eq!(draft.kind, ComposeKind::Forward);
    assert_eq!(draft.to, "", "a forward is addressed by the sender");

    // A frame for the card's own to take the keyboard, then the typing.
    run_app_frame(&ctx, &mut app, Vec::new());
    run_app_frame(
        &ctx,
        &mut app,
        vec![egui::Event::Text("bob@example.com".to_string())],
    );
    let draft = app.compose.as_ref().expect("the draft is still open");
    assert_eq!(draft.to, "bob@example.com", "the typing landed in To");
    assert!(
        draft.body.starts_with("On "),
        "and not in the body: {:?}",
        draft.body
    );
}

/// The files come after the mail the answer says and not before it: the
/// body is what the card is for and what the mail above it is answered
/// with, and a file is something put *with* it. The popup keeps its form's
/// order — files over body — where the row of attachments belongs to the
/// fields above it rather than to the letter below them.
#[test]
fn the_files_row_waits_under_the_mail_written_on_the_spot() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());
    click_at(&ctx, &mut app, header_action_center(4));
    let _ = frame_texts(&ctx, &mut app);

    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let answer = *pane_cards(&ctx, &mut app)
        .last()
        .expect("the answer is a card of the pane");
    // The body is the one field the card draws as a box (the scheme's
    // input fill, `compose_body`); the attachments' own row is the box
    // under it, found by the words the box carries — the card draws no
    // label beside it.
    let body = *fills_in_color(&shapes, theme::of(&ctx).input)
        .iter()
        .find(|rect| answer.contains(rect.center()))
        .expect("the body is on the card");
    let files = text_boxes_of(
        &shapes,
        answer,
        "Click to select attachments, or drag files here",
    )
    .into_iter()
    .next()
    .expect("the attachments row is on the card");
    assert!(
        files.top() > body.bottom(),
        "the files wait under the mail: files {files:?} of body {body:?}"
    );
}

/// The body is the whole writing area and not the dozen lines an empty
/// draft happens to need: the box fills `COMPOSE_BODY_HEIGHT`, the room
/// the card gives it, so the row under it is not read across a band of
/// the card's own surface. It is the card's whole column too — padding to
/// padding, the width the head bar and the mail above it read at — rather
/// than the field column the labelled rows use, so the mail being written
/// is as wide as the card will hold. The box still stops at that height
/// when the mail outgrows it — that is the scroll area's own `max_height`
/// — and what it holds scrolls under the box rather than the box growing
/// with it (`compose_body`).
#[test]
fn the_answers_body_fills_the_writing_area() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());
    click_at(&ctx, &mut app, header_action_center(4));
    let _ = frame_texts(&ctx, &mut app);

    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let answer = *pane_cards(&ctx, &mut app)
        .last()
        .expect("the answer is a card of the pane");
    let body = *fills_in_color(&shapes, theme::of(&ctx).input)
        .iter()
        .find(|rect| answer.contains(rect.center()))
        .expect("the body is on the card");
    assert!(
        (body.height() - COMPOSE_BODY_HEIGHT).abs() < 1.0,
        "the box is the writing area, {COMPOSE_BODY_HEIGHT} tall: {body:?}"
    );
    assert!(
        (body.width() - (answer.width() - CONTENT_PADDING.x * 2.0)).abs() < 1.0,
        "the box is the card's whole column: {body:?} of {answer:?}"
    );
}

/// The card ends with its own content: the buttons are the last row on it
/// and its surface closes `CONTENT_PADDING` under them. The row is a
/// bounded one (`ui.horizontal`, a row the height of what is in it) — a
/// `with_layout` over the space left would center the pair in the whole
/// of it and carry the card's surface down to the pane's floor, which is
/// what left the empty band between the reason and the buttons.
#[test]
fn the_answers_card_ends_where_its_content_does() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());
    click_at(&ctx, &mut app, header_action_center(4));
    let _ = frame_texts(&ctx, &mut app);

    // The fixture account cannot send, so the reason line is there too:
    // the row above the buttons is the worst case for the card's end.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let answer = *pane_cards(&ctx, &mut app)
        .last()
        .expect("the answer is a card of the pane");
    let scheme = theme::of(&ctx);
    let buttons: Vec<egui::Rect> = fills_in_color(&shapes, scheme.card)
        .into_iter()
        .filter(|rect| {
            (rect.height() - COMPOSE_BUTTON_HEIGHT).abs() < 0.5 && answer.contains(rect.center())
        })
        .collect();
    assert_eq!(buttons.len(), 2, "Send and Discard: {buttons:?}");
    let bottom = buttons.iter().fold(f32::NEG_INFINITY, |bottom, button| {
        bottom.max(button.bottom())
    });
    assert!(
        (answer.bottom() - bottom - CONTENT_PADDING.y).abs() < 0.5,
        "the card closes under the buttons: card {answer:?} of {buttons:?}"
    );
}

/// The writing area's box is the card's, not the text's: it stands on the
/// room the card gives the body — `COMPOSE_BODY_HEIGHT` of it — with its
/// line drawn on all four sides of that box, however long the mail is. The
/// fixtures' answers quote one line, but an answer quotes a *mail*, and a
/// text edit's own frame follows what it holds: a frame taller than the
/// viewport runs its ends outside the room, where the scroll takes them —
/// the top out of sight after a scroll down, the bottom before one.
#[test]
fn the_answers_body_box_is_the_writing_area_and_not_the_mail() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());
    click_at(&ctx, &mut app, header_action_center(4));
    // A mail longer than the room the card gives it: what a quoted mail
    // is, and what no fixture's own answer is.
    app.compose.as_mut().expect("the reply opened a draft").body =
        (0..60).map(|line| format!("> line {line}\n")).collect();
    for _ in 0..2 {
        run_app_frame(&ctx, &mut app, Vec::new());
    }

    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let answer = *pane_cards(&ctx, &mut app)
        .last()
        .expect("the answer is a card of the pane");
    // The writing area is the one field the card draws as a box: the
    // scheme's own input fill. It is looked for as the box the card's
    // middle sits in, so a box that grew with the mail is found too —
    // which is what the field's own frame used to be — and is measured
    // below.
    let body = *fills_in_color(&shapes, theme::of(&ctx).input)
        .iter()
        .find(|rect| rect.contains(answer.center()))
        .expect("the body is on the card");
    assert!(
        (body.height() - COMPOSE_BODY_HEIGHT).abs() < 1.0,
        "the box is the writing area, not the mail: {body:?}"
    );
    let boxes = stroked_boxes_in(&shapes, answer);
    assert!(
        boxes.iter().any(|(rect, _)| *rect == body),
        "and its line is drawn on that box: {boxes:?} of {body:?}"
    );
}

/// The reply's To row ends with the chevron that folds the Cc and Bcc
/// lines out under it: down while they are away, up once they are under
/// the row. An answer that needs no copies shows the one address it goes
/// to, and a Reply all's own Cc is one click from being read — and
/// edited — rather than only being copied in the dark.
#[test]
fn the_replys_to_row_folds_cc_and_bcc_out() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    // A mail with two recipients, one of them the user's own address, so
    // a Reply all has somebody else to copy.
    if let Some(account) = app.accounts.first_mut() {
        account.username = "me@example.com".to_string();
    }
    {
        let mail = &mut app
            .states
            .get_mut("acct")
            .expect("the fixture account")
            .mailbox
            .folders[0]
            .messages[0];
        mail.to = "me@example.com, Bob <bob@example.com>".to_string();
    }
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());

    // Reply all: To the sender, Cc the other recipient — folded away.
    click_at(&ctx, &mut app, header_action_center(3));
    let draft = app.compose.as_ref().expect("the reply opened a draft");
    assert_eq!(app.compose_spot, ComposeSpot::Pane);
    assert_eq!(draft.kind, ComposeKind::ReplyAll);
    assert_eq!(draft.cc, "bob@example.com", "the copy is in the draft");
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts.iter().any(|text| text == "To:"),
        "the row's label is a letter's: {texts:?}"
    );
    for absent in ["Cc:", "Bcc:", "bob@example.com"] {
        assert!(
            !texts.iter().any(|text| text == absent),
            "{absent} is folded away at first: {texts:?}"
        );
    }

    // The chevron at the end of the row: a line drawing pointing down.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let answer = *pane_cards(&ctx, &mut app)
        .last()
        .expect("the answer is a card of the pane");
    let fold = fold_chevron(&shapes, answer);
    assert_eq!(fold.len(), 3, "one chevron on the answer: {fold:?}");
    assert!(
        fold[1].y > fold[0].y && fold[1].y > fold[2].y,
        "it points down while the copies are away: {fold:?}"
    );

    // A click folds them out: the two labels, and what the draft holds.
    let chevron = egui::Rect::from_points(&fold).center();
    click_at(&ctx, &mut app, chevron);
    let texts = frame_texts(&ctx, &mut app);
    for shown in ["Cc:", "Bcc:", "bob@example.com"] {
        assert!(
            texts.iter().any(|text| text == shown),
            "{shown} is out under the To row: {texts:?}"
        );
    }
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let fold = fold_chevron(&shapes, answer);
    assert!(
        fold[1].y < fold[0].y && fold[1].y < fold[2].y,
        "it points up once they are under the row: {fold:?}"
    );

    // And the same click folds them away again.
    click_at(&ctx, &mut app, chevron);
    let texts = frame_texts(&ctx, &mut app);
    for absent in ["Cc:", "Bcc:", "bob@example.com"] {
        assert!(
            !texts.iter().any(|text| text == absent),
            "{absent} is folded away again: {texts:?}"
        );
    }
}

/// The answer's From list ends with the chevron its To row ends with rather
/// than the solid triangle a stock combo box carries: the head bar is a
/// letter's line, so the two rows of the answer that end in something to
/// open are drawn alike — one line drawing, down while what it holds is
/// away and up while it is out. The popup's own From keeps egui's triangle:
/// there the card is a form of boxes, and each box wears that form's own
/// marks.
#[test]
fn the_answers_from_list_ends_with_the_to_rows_chevron() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());
    // Reply: the answer is written under its mail, on the spot.
    click_at(&ctx, &mut app, header_action_center(4));
    assert_eq!(app.compose_spot, ComposeSpot::Pane);

    // The card's two line drawings, top to bottom: the From list's own
    // chevron on the head's first line, and the To row's under it. The head
    // is the card down to the To row's chevron, and the From list's is the
    // only glyph drawn in there: a line drawing, pointing down while the
    // list is away — no filled triangle standing in its place on the head
    // of a letter.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let answer = *pane_cards(&ctx, &mut app)
        .last()
        .expect("the answer is a card of the pane");
    let chevrons = chevrons_in(&shapes, answer);
    assert_eq!(chevrons.len(), 2, "one on each row: {chevrons:?}");
    let head = egui::Rect::from_min_max(answer.min, egui::pos2(answer.max.x, chevrons[1][0].y));
    let from = chevrons_in(&shapes, head);
    assert_eq!(from.len(), 1, "one chevron on the head: {from:?}");
    assert!(
        from[0][1].y > from[0][0].y && from[0][1].y > from[0][2].y,
        "it points down while the list is away: {from:?}"
    );
    let triangle = triangles_in(&shapes, head);
    assert!(
        triangle.is_empty(),
        "and no solid triangle stands in its place: {triangle:?}"
    );
    assert!(
        chevrons[1][0].y > chevrons[0][0].y,
        "the second is the To row's, under the first: {chevrons:?}"
    );
    let (first, second) = (
        egui::Rect::from_points(&chevrons[0]).size(),
        egui::Rect::from_points(&chevrons[1]).size(),
    );
    assert!(
        (first.x - second.x).abs() < 0.5 && (first.y - second.y).abs() < 0.5,
        "one drawing, on both rows: {chevrons:?}"
    );

    // It is drawn the way the row under it is drawn too: the scheme's
    // weak text while the pointer is away, its own text color under the
    // hand — the flat list's hover fill is what says the button answers
    // the pointer at all.
    let from_chevron = egui::Rect::from_points(&from[0]).center();
    let scheme = theme::of(&ctx);
    let rest = glyph_colors_in(&shapes, head);
    assert!(
        rest.contains(&scheme.text_weak) && !rest.contains(&scheme.text),
        "the From chevron is weak while the pointer is away: {rest:?}"
    );
    let lit = glyph_colors_in(
        &frame_shapes_with(
            &ctx,
            &mut app,
            1400.0,
            vec![egui::Event::PointerMoved(from_chevron)],
        ),
        head,
    );
    assert!(
        lit.contains(&scheme.text) && !lit.contains(&scheme.text_weak),
        "the pointer brings it to the text color, as it brings the To row's: {lit:?}"
    );

    // A click on it opens the list under the row — the combo is still the
    // combo, with the drawing changed — and the chevron turns up while the
    // list is out, the way the To row's turns over the lines it folds out.
    let label = "Test <ada>";
    let count = |ctx: &egui::Context, app: &mut KimoApp| {
        frame_texts(ctx, app)
            .into_iter()
            .filter(|text| text == label)
            .count()
    };
    assert_eq!(count(&ctx, &mut app), 1, "the list is away at first");
    click_at(&ctx, &mut app, from_chevron);
    assert_eq!(
        count(&ctx, &mut app),
        2,
        "the click opened the list under the row"
    );
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let out = chevrons_in(&shapes, head);
    assert_eq!(out.len(), 1, "still one chevron on the head bar: {out:?}");
    assert!(
        out[0][1].y < out[0][0].y && out[0][1].y < out[0][2].y,
        "it points up while the list is under the row: {out:?}"
    );

    // Another click puts it away again, and the drawing is back where it
    // started: the same two states the To row's own chevron has.
    click_at(&ctx, &mut app, from_chevron);
    assert_eq!(count(&ctx, &mut app), 1, "the click closed the list");
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let shut = chevrons_in(&shapes, head);
    assert_eq!(shut.len(), 1, "one chevron on the head bar: {shut:?}");
    assert!(
        shut[0][1].y > shut[0][0].y && shut[0][1].y > shut[0][2].y,
        "it points down again: {shut:?}"
    );

    // The popup is the same card in its other container, and its From list
    // ends the same way: the chevron, with no filled triangle standing in
    // for it.
    let mail = app.selected_mail().cloned().expect("the mail to answer");
    app.compose_about(MailAction::Reply, ComposeSpot::Popup, &mail);
    for _ in 0..FADE_FRAMES {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let from_label = text_boxes_of(&shapes, popup_area(), "From:")
        .into_iter()
        .next()
        .expect("the popup's own From row");
    // The row the label is on, out to the card's own width — the label is
    // right-aligned in the card's label column.
    let row = egui::Rect::from_min_max(
        egui::pos2(
            from_label.right() - COMPOSE_LABEL_WIDTH,
            from_label.top() - 2.0,
        ),
        egui::pos2(
            from_label.right() - COMPOSE_LABEL_WIDTH + COMPOSE_WIDTH,
            from_label.bottom() + 2.0,
        ),
    );
    assert_eq!(
        chevrons_in(&shapes, row).len(),
        1,
        "the popup's From list ends with the chevron"
    );
    assert!(
        triangles_in(&shapes, row).is_empty(),
        "and no filled triangle stands in its place"
    );
}
/// The on-the-spot card is a letter, not a form: it draws no rule at all —
/// neither under its head nor over its foot — so the mail above it and its
/// own rows are what its ends are read against. What the card has to say
/// (here, why Send cannot be pressed) stands on a row of its own, over the
/// two buttons rather than beside them.
#[test]
fn the_answer_draws_no_rule_and_says_its_piece_over_the_buttons() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());
    click_at(&ctx, &mut app, header_action_center(4));
    let _ = frame_texts(&ctx, &mut app);

    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let answer = *pane_cards(&ctx, &mut app)
        .last()
        .expect("the answer is a card of the pane");
    let scheme = theme::of(&ctx);

    // No rule on the card at all: the head is the letter's own first lines
    // and the foot is a letter's too, so the mail above the card is what
    // both of its ends are read against.
    let rules = horizontal_rules(&shapes, answer);
    assert!(
        rules.is_empty(),
        "the answer draws no rule: {rules:?} of {answer:?}"
    );

    // What the card says is on its own row, clear above the buttons.
    let reason = send_blocker(
        app.accounts.first(),
        &["ada@example.com".to_string()],
        &[],
        &[],
    )
    .expect("the fixture account has no outgoing server");
    let hint = text_boxes_of(&shapes, answer, &reason)
        .into_iter()
        .next()
        .expect("the reason is on the card");
    let buttons: Vec<egui::Rect> = fills_in_color(&shapes, scheme.card)
        .into_iter()
        .filter(|rect| {
            (rect.height() - COMPOSE_BUTTON_HEIGHT).abs() < 0.5 && answer.contains(rect.center())
        })
        .collect();
    assert_eq!(buttons.len(), 2, "Send and Discard: {buttons:?}");
    let buttons_top = buttons
        .iter()
        .fold(f32::INFINITY, |top, button| top.min(button.top()));
    assert!(
        hint.bottom() <= buttons_top + 0.5,
        "the reason stands over the buttons: {hint:?} of {buttons:?}"
    );
}

/// The card's two buttons say their state in their word, not only in the
/// fill: Discard's turns white under the pointer — the danger fill beneath
/// it needs that in either scheme — and a disabled Send's is greyed with the
/// rest of the disabled treatment.
#[test]
fn the_answers_buttons_paint_their_word_in_their_ink() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());
    click_at(&ctx, &mut app, header_action_center(4));

    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let answer = *pane_cards(&ctx, &mut app)
        .last()
        .expect("the answer is a card of the pane");
    let scheme = theme::of(&ctx);
    // The fixture's account has no outgoing server, so Send is the disabled
    // one: its word is the scheme's weak text, while Discard's waits in the
    // card's own text.
    assert_eq!(
        text_color_in(&shapes, answer, "Send"),
        Some(scheme.text_weak),
        "a disabled Send's word is greyed"
    );
    assert_eq!(
        text_color_in(&shapes, answer, "Discard"),
        Some(scheme.text),
        "Discard waits in the card's own text"
    );

    // Under the pointer Discard's word is white — `on_danger`, the ink its
    // danger fill takes in both schemes.
    let discard = text_boxes_of(&shapes, answer, "Discard")
        .into_iter()
        .next()
        .expect("the Discard word");
    let _ = frame_shapes_with(
        &ctx,
        &mut app,
        1400.0,
        vec![egui::Event::PointerMoved(discard.center())],
    );
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert_eq!(
        text_color_in(&shapes, answer, "Discard"),
        Some(scheme.on_danger),
        "the pointer turns Discard's word white on its danger fill"
    );
    assert!(
        fills_in_color(&shapes, scheme.danger_hover)
            .iter()
            .any(|rect| rect.contains(discard.center())),
        "and its fill is the danger one: {discard:?}"
    );
}

/// The card's own two open its foot's row rather than ending it: Discard
/// first and Send beside it, against the card's left edge — the side the
/// mail above is read from — where the popup keeps its stock pair at its
/// form's right end. Send is the accent-filled one of the two
/// (`compose_button`), and a settled draft is what lets it be: the fixture
/// account is given an outgoing server so the pair is the enabled pair.
#[test]
fn the_answers_buttons_open_the_foot_at_the_cards_left_edge() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    if let Some(account) = app.accounts.first_mut() {
        account.username = "ada@example.com".to_string();
        account.smtp = Some(SmtpSettings {
            host: "smtp.example.com".to_string(),
            port: 587,
            tls: TlsMode::StartTls,
            login: None,
        });
    }
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());
    click_at(&ctx, &mut app, header_action_center(4));
    let _ = frame_texts(&ctx, &mut app);

    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let answer = *pane_cards(&ctx, &mut app)
        .last()
        .expect("the answer is a card of the pane");
    let scheme = theme::of(&ctx);
    let button_in = |fill: egui::Color32| -> egui::Rect {
        let mut found = fills_in_color(&shapes, fill).into_iter().filter(|rect| {
            (rect.height() - COMPOSE_BUTTON_HEIGHT).abs() < 0.5 && answer.contains(rect.center())
        });
        let button = found.next().expect("a button of that fill");
        assert!(found.next().is_none(), "one button of that fill");
        button
    };
    let send = button_in(scheme.accent);
    let discard = button_in(scheme.card);

    assert!(
        discard.left() < send.left(),
        "Discard comes first: {discard:?} of {send:?}"
    );
    assert!(
        (discard.left() - (answer.left() + CONTENT_PADDING.x)).abs() < 0.5,
        "the pair opens the card's own column: {discard:?} of {answer:?}"
    );
}

/// A compose opened on the spot stays while its mail is on the pane; once
/// the mail has left it — another folder opened under it, whose own first
/// move is to drop the selection — the draft moves to the popup window:
/// still a draft, in the container that needs no mail under it.
#[test]
fn an_answer_leaves_the_pane_with_its_mail() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());
    click_at(&ctx, &mut app, header_action_center(4));
    assert_eq!(app.compose_spot, ComposeSpot::Pane);

    app.selected_message = None;
    run_app_frame(&ctx, &mut app, Vec::new());
    assert_eq!(app.compose_spot, ComposeSpot::Popup);
    assert!(app.compose.is_some(), "the move kept the draft");
}

// ----- labels ------------------------------------------------------------

/// A file dropped on the compose card is attached to the draft: the card
/// draws the name and the size, a path typed into its box is the other way
/// in, a path that is not a file says so rather than attaching nothing, and
/// a row's own Remove takes that one back off.
#[test]
fn a_file_dropped_on_the_compose_card_is_attached() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.open_compose();
    // The card is an `Area` like every other popup: it lays itself out on
    // the frame it is opened on, so the frame a drop lands in is the next
    // one.
    run_app_frame(&ctx, &mut app, Vec::new());

    let dir = std::env::temp_dir().join("kimo-compose-drop");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("report.pdf");
    std::fs::write(&path, b"a report").unwrap();

    drop_files(&ctx, &mut app, vec![path.clone()]);
    let draft = app.compose.as_ref().expect("the card is open");
    assert_eq!(draft.attachments.len(), 1, "the drop attached it");
    assert_eq!(draft.attachments[0].name, "report.pdf");
    assert!(draft.attach_error.is_none(), "{:?}", draft.attach_error);

    // The card says what it holds.
    let texts = frame_texts(&ctx, &mut app);
    assert!(texts.iter().any(|run| run == "report.pdf"), "{texts:?}");
    assert!(
        texts.iter().any(|run| run == "application/pdf · 8 B"),
        "the card's second line, as the reading pane draws it: {texts:?}"
    );

    // The card that says what to do with a file is one area, and it says both
    // ways in: a click opens the desktop's dialog, and a file dragged from a
    // file manager lands on the window and is attached by `attach_dropped`.
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts
            .iter()
            .any(|run| run == "Click to select attachments, or drag files here"),
        "{texts:?}"
    );
    // And while the desktop's dialog is open, the card says so — and nothing
    // else on it can be pressed.
    app.picking_files = true;
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts.iter().any(|run| run == "Choosing attachments…"),
        "{texts:?}"
    );
    app.picking_files = false;

    // A path that is not a file says so — under the card, where the reason
    // for the last file that did not attach is drawn — and attaches nothing.
    // (Reached through `attach_file` rather than by clicking the card: a
    // click opens the desktop's dialog, which is not a test's business.)
    let missing = dir.join("gone.pdf");
    let draft = app.compose.as_mut().expect("open");
    assert!(!attach_file(draft, missing.clone()), "nothing was attached");
    assert_eq!(draft.attachments.len(), 1, "the file that was there stays");
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts.iter().any(|run| run.contains("gone.pdf")),
        "the reason names the path: {texts:?}"
    );

    // And a path that is a file goes on: the same file twice is two parts, in
    // the order they were attached.
    let draft = app.compose.as_mut().expect("open");
    assert!(attach_file(draft, path.clone()), "the file attaches");
    assert_eq!(draft.attachments.len(), 2);
    assert!(draft.attach_error.is_none(), "and the refusal is gone");

    // Every file card carries a small cross in its right-hand gutter — two
    // strokes whose midpoints are the button's centre — and a click on the
    // cross takes that file off the mail. It is found by the strokes it drew
    // rather than by working out where it ought to be.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let area = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
    let cards = attachment_card_rects(&shapes, area);
    assert_eq!(cards.len(), 2, "one card per file: {cards:?}");
    let centres = stroke_centres(&shapes, area);
    let crosses: Vec<egui::Pos2> = cards
        .iter()
        .filter_map(|card| {
            let wanted = egui::pos2(
                card.right() - ATTACHMENT_CARD_PADDING - ATTACHMENT_CLOSE_SIZE / 2.0,
                card.center().y,
            );
            centres
                .iter()
                .copied()
                .find(|centre| centre.distance(wanted) < 1.0)
        })
        .collect();
    assert_eq!(crosses.len(), 2, "a cross on each card: {centres:?}");

    click_at(&ctx, &mut app, crosses[0]);
    assert_eq!(
        app.compose.as_ref().expect("open").attachments.len(),
        1,
        "the cross took that one off, and the other stays"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
