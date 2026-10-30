use super::super::*;
use super::common::*;

/// Developer tool (there is no compositor at hand in a test): draw a few
/// frames — the labelled list and card, the label filter and popup, the
/// flagged list, the list filter's menu and the list it leaves, the sort
/// menu, the bar with and without a mail open in the pane, and
/// Preferences → General, Appearance, Graphics and About — and write them out as PNGs,
/// so the layout can be
/// looked at. `KIMO_UI_DUMP` picks the directory (default `target`) and
/// `KIMO_UI_WIDTH` the window's width (default 1400).
///
///     cargo test -- --ignored --nocapture dump_ui
#[test]
#[ignore = "developer tool: writes PNGs of a few frames and asserts nothing"]
fn dump_ui() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_two_accounts(&ctx, 3);
    label_mail(&mut app, "acct", "INBOX", "acct-0", "Work");
    label_mail(&mut app, "acct", "INBOX", "acct-1", "Work");
    label_mail(&mut app, "acct", "INBOX", "acct-1", "Travel");
    label_mail(&mut app, "other", "INBOX", "other-0", "Travel");
    app.selected_message = Some("acct-1".to_string());
    // The first frame the app draws text on is the one that uploads the
    // font atlas, and every frame after it only patches what is there: the
    // rasterizer has to be in on that frame, so the textures outlive it.
    let mut textures = HashMap::new();
    collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    let dir = std::path::PathBuf::from(
        std::env::var("KIMO_UI_DUMP").unwrap_or_else(|_| "target".to_string()),
    );
    let shot = |app: &mut KimoApp, name: &str, textures: &mut HashMap<_, _>| {
        let path = dir.join(name);
        render_frame_png(&ctx, app, &path, textures);
        println!("wrote {}", path.display());
    };
    shot(&mut app, "ui-labels-card.png", &mut textures);
    // The label row in the column, found where the frame drew it. A window
    // narrow enough for the navigation to have collapsed has no column to
    // find it in: the label states then simply are not part of the dump,
    // which is what looking at a narrow *bar* wants anyway.
    let shapes = collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    let row = text_boxes_of(&shapes, nav_column(&app), "Work")
        .into_iter()
        .next();
    if let Some(row) = row {
        dump_click(&ctx, &mut app, &mut textures, row.center());
        shot(&mut app, "ui-labels-filter.png", &mut textures);
        dump_click(&ctx, &mut app, &mut textures, header_action_center(1));
        shot(&mut app, "ui-labels-popup.png", &mut textures);
    }
    // And Preferences → General, where the sync settings live. The label
    // popup is dismissed first — a menu left open over the columns would be
    // half the picture — and the modal itself is an `Area`, which lays
    // itself out on the frame it is opened on and draws on the next: one
    // frame first, or the picture would be of a window with no modal in it.
    egui::Popup::close_all(&ctx);
    // And the flag: the marked row in the list — warning fill, flag glyph,
    // and a paperclip beside it to show the two marks sharing the end of a
    // title line — with a marked mail open in the pane, whose card is the
    // plain surface it always was.
    app.set_flag_on_mail(&MailKey::new("acct", "INBOX", "acct-0"), true);
    app.set_flag_on_mail(&MailKey::new("acct", "INBOX", "acct-1"), true);
    {
        let state = app.states.get_mut("acct").expect("the fixture account");
        state.mailbox.folders[0].messages[1].attachments = vec![Attachment {
            name: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size: 4096,
        }];
        state.folders_changed();
    }
    for _ in 0..16 {
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    }
    shot(&mut app, "ui-flag.png", &mut textures);

    // And the row's own two buttons under the pointer: the flag cell just
    // inside the delete one — wearing the mark's colour on the mail that is
    // flagged — and then the flag cell itself under the pointer, filled with
    // the tint a flagged row is filled with.
    let row = checkbox_center(&app, 1, false);
    for _ in 0..2 {
        collect_frame(
            &ctx,
            &mut app,
            &mut textures,
            vec![egui::Event::PointerMoved(egui::pos2(320.0, row.y))],
        );
    }
    shot(&mut app, "ui-row-buttons.png", &mut textures);
    let flag_cell = row_flag_center(&app, 1);
    for _ in 0..2 {
        collect_frame(
            &ctx,
            &mut app,
            &mut textures,
            vec![egui::Event::PointerMoved(flag_cell)],
        );
    }
    shot(&mut app, "ui-row-flag-hover.png", &mut textures);

    // And the filter: the checklist over the list it narrows — every box
    // clear but "All" — then two of them ticked, then the list itself with
    // the menu out of the way and the cell in the accent that says a view is
    // on.
    let filter_cell = titlebar_filter_rect(&ctx, &mut app, 1400.0).center();
    dump_click(&ctx, &mut app, &mut textures, filter_cell);
    for _ in 0..2 {
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    }
    shot(&mut app, "ui-filter-menu.png", &mut textures);
    // The boxes are independent, and a tick leaves the menu open: the
    // picture is of the AND before it is applied to the list.
    let tick = |app: &mut KimoApp, textures: &mut HashMap<_, _>, label: &str| {
        let shapes = collect_frame(&ctx, app, textures, Vec::new());
        let area = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(dump_width(), DUMP_HEIGHT as f32),
        );
        let row = text_boxes_of(&shapes, area, label)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("the menu draws no {label} row"));
        dump_click(&ctx, app, textures, row.center());
    };
    tick(&mut app, &mut textures, "Unread");
    tick(&mut app, &mut textures, "Has files");
    for _ in 0..2 {
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    }
    shot(&mut app, "ui-filter-and.png", &mut textures);
    // The menu goes before the picture of the list itself: in the app it
    // closes on a click outside, and what is being looked at here is what
    // the list became — one view, the folder's own.
    egui::Popup::close_all(&ctx);
    app.set_list_filters(ListFilters(vec![ListFilter::Flagged]));
    for _ in 0..2 {
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    }
    shot(&mut app, "ui-filter-flagged.png", &mut textures);
    app.set_list_filters(ListFilters::default());

    // And an answer written on the spot: the card under its own mail, whose
    // head bar is the account's avatar and the From list — no heading, and no
    // Cc, Bcc or Subject. The account is given an outgoing server and an
    // address to send as first, so the shot shows the footer a working
    // account has: the accent-filled Send with the plane in its own ink,
    // rather than the muted pair a blocked draft draws.
    {
        if let Some(account) = app.accounts.first_mut() {
            account.username = "ada@example.com".to_string();
            account.smtp = Some(SmtpSettings {
                host: "smtp.example.com".to_string(),
                port: 587,
                tls: TlsMode::StartTls,
                login: None,
            });
        }
        let mail = app.selected_mail().cloned().or_else(|| {
            app.states.get("acct").and_then(|state| {
                state
                    .mailbox
                    .folders
                    .first()
                    .and_then(|folder| folder.messages.first().cloned())
            })
        });
        if let Some(mail) = mail {
            app.compose_about(MailAction::Reply, ComposeSpot::Pane, &mail);
            for _ in 0..4 {
                collect_frame(&ctx, &mut app, &mut textures, Vec::new());
            }
            shot(&mut app, "ui-reply-on-the-spot.png", &mut textures);
            // And Discard under the pointer: its word turns white on the
            // danger fill, where the Send beside it keeps its own ink
            // (`compose_button`).
            let card = *pane_cards(&ctx, &mut app)
                .last()
                .expect("the answer is a card of the pane");
            let shapes = collect_frame(&ctx, &mut app, &mut textures, Vec::new());
            let discard = text_boxes_of(&shapes, card, "Discard")
                .into_iter()
                .next()
                .expect("the Discard word");
            for _ in 0..2 {
                collect_frame(
                    &ctx,
                    &mut app,
                    &mut textures,
                    vec![egui::Event::PointerMoved(discard.center())],
                );
            }
            shot(&mut app, "ui-reply-discard-hover.png", &mut textures);
            app.compose = None;
        }
    }

    // And the compose card with a file on it: the row the attachment takes,
    // its size, the Remove, and the box that is the other way in.
    {
        app.compose = Some(ComposeDraft::new(Some("acct".to_string())));
        if let Some(draft) = app.compose.as_mut() {
            draft.to = "bob@example.com".to_string();
            draft.subject = "The report".to_string();
            draft.body = "Here it is.\n".to_string();
            let path = std::env::temp_dir().join("kimo-dump-report.pdf");
            let _ = std::fs::write(&path, vec![b'%'; 4096]);
            let _ = attach_file(draft, path);
        }
        for _ in 0..4 {
            collect_frame(&ctx, &mut app, &mut textures, Vec::new());
        }
        shot(&mut app, "ui-compose-files.png", &mut textures);
        app.compose = None;
    }

    // And the popup's own form: the same head as the answer on the spot —
    // the account's face, `From:`, `To:` with its fold chevron, and
    // `Subject:` — over the card's writing, its attachments and its own two
    // buttons, with the fields quiet (a line only where the pointer or the
    // keyboard is, `docs/ui-style.md`).
    {
        let spot = app.compose_spot;
        app.compose = Some(ComposeDraft::new(Some("acct".to_string())));
        app.compose_spot = ComposeSpot::Popup;
        if let Some(draft) = app.compose.as_mut() {
            draft.to = "bob@example.com".to_string();
            draft.subject = "The report".to_string();
            draft.body = "Here it is.\n".to_string();
        }
        for _ in 0..4 {
            collect_frame(&ctx, &mut app, &mut textures, Vec::new());
        }
        shot(&mut app, "ui-compose-popup.png", &mut textures);
        app.compose = None;
        app.compose_spot = spot;
    }

    // And the sort cell's menu: the order group above the by group, each a
    // choice of one and ticked at the end of the row in force.
    let sort_cell = titlebar_sort_rect(&ctx, &mut app, 1400.0).center();
    dump_click(&ctx, &mut app, &mut textures, sort_cell);
    for _ in 0..2 {
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    }
    shot(&mut app, "ui-sort-menu.png", &mut textures);
    egui::Popup::close_all(&ctx);

    // And the search popup: the bar with the magnifier inside its left edge
    // and the round cross inside its right once there is a query to clear,
    // over the scope combo and the filter grid — with no heading, no close
    // button and no Search button, because the query runs as it is typed.
    app.open_search();
    for _ in 0..FADE_FRAMES {
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    }
    shot(&mut app, "ui-search-empty.png", &mut textures);
    app.search.text = "ada".to_string();
    for _ in 0..2 {
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    }
    shot(&mut app, "ui-search.png", &mut textures);
    app.search = SearchQuery::default();
    app.search_hits.clear();
    app.search_running = false;
    app.show_search = false;
    for _ in 0..FADE_FRAMES {
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    }

    // The bar with nothing open in the pane first: the pane's run is not
    // there at all rather than there and dimmed, and the chrome it would
    // have used is the drag surface. The frames are what its fade needs.
    app.selected_message = None;
    for _ in 0..16 {
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    }
    shot(&mut app, "ui-bar-no-mail.png", &mut textures);
    app.selected_message = Some("acct-1".to_string());

    app.show_preferences = true;
    app.prefs_tab = PrefsTab::General;
    collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    shot(&mut app, "ui-general.png", &mut textures);
    // And Preferences → Appearance, where the theme's radios, the accent's
    // chips and the palette they resolve to are: the whole page at once,
    // which is the one place a colour can be chosen.
    app.prefs_tab = PrefsTab::Appearance;
    for _ in 0..2 {
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    }
    shot(&mut app, "ui-appearance.png", &mut textures);
    // And Preferences → Graphics: the cards this machine has and the
    // drivers a pick opens. The scan is a worker's job in the app; here it
    // runs inline, so the picture is of the page it fills in, with the
    // machine's first card picked.
    app.gpu_scan = Some(crate::gpus::scan());
    app.gpu = crate::gpus::cards().first().map(|card| card.pci.clone());
    app.prefs_tab = PrefsTab::Graphics;
    for _ in 0..2 {
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    }
    shot(&mut app, "ui-graphics.png", &mut textures);
    // And Preferences → About: the icon, the build's version and the
    // summary, the one page that configures nothing.
    app.prefs_tab = PrefsTab::About;
    for _ in 0..2 {
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    }
    shot(&mut app, "ui-about.png", &mut textures);
    app.show_preferences = false;

    // And the Blitz page's paper: the sender's page on white, and on the dark
    // surface after the contrast cell on the mail's head. The page is a test
    // one — a real render is a worker's job, not a dump's — and its tile is
    // transparent, the way a mail that paints no background of its own
    // rasterizes, so what these two shots show is the paper itself.
    app.html_renderer = HtmlRenderer::Blitz;
    {
        let state = app.states.get_mut("acct").expect("the fixture account");
        state.mailbox.folders[0].messages[1].body_html = Some("<p>The mail.</p>".to_string());
    }
    app.blitz_pages.clear();
    app.blitz_order.clear();
    app.pending_blitz_key = None;
    for _ in 0..2 {
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    }
    if let Some((key, _)) = app.pending_blitz_key.clone() {
        let (page, _) =
            ready_blitz_page(&ctx, egui::vec2(900.0, 260.0), egui::Color32::TRANSPARENT);
        app.blitz_pages.insert(key, page);
    }
    for _ in 0..2 {
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    }
    shot(&mut app, "ui-blitz-paper.png", &mut textures);
    dump_click(&ctx, &mut app, &mut textures, header_action_center(5));
    for _ in 0..2 {
        collect_frame(&ctx, &mut app, &mut textures, Vec::new());
    }
    shot(&mut app, "ui-blitz-paper-dark.png", &mut textures);
    app.html_renderer = HtmlRenderer::Simple;
}
