use super::super::*;
use super::common::*;

/// A folder the account has not fetched yet — INBOX, offered by the
/// navigation before the first sync — is empty as far as the list is
/// concerned, and the column has to say which kind of empty it is rather
/// than standing bare. (It stands bare if the list gives up on a folder it
/// cannot find.)
#[test]
fn an_unfetched_folder_says_so_instead_of_standing_empty() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let account = Account {
        id: "acct".to_string(),
        name: "Test".to_string(),
        protocol: Protocol::Imap,
        host: "imap.example.com".to_string(),
        port: 993,
        username: "ada".to_string(),
        password: "secret".to_string(),
        tls: TlsMode::Implicit,
        smtp: None,
        provider: None,
        provider_config: None,
        access_token: None,
        refresh_token: None,
        token_expires_at: None,
    };
    // No folders at all: the account has never synced.
    let mut app = KimoApp::assemble(
        &ctx,
        vec![account],
        HashMap::new(),
        &storage::Settings::default(),
        std::sync::Arc::new(crate::plugin::discovery::Registry::empty()),
    );
    assert_eq!(app.selected_folder.as_deref(), Some("INBOX"));
    assert!(app.folder_messages_of("acct", "INBOX").is_none());
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts.iter().any(|text| text.contains("Not fetched yet")),
        "the column did not say the folder is unfetched: {texts:?}"
    );

    // A folder that has synced, with no mail in it, says the other thing.
    let mut app = app_with_messages(&ctx, 0);
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts
            .iter()
            .any(|text| text.contains("This folder is empty.")),
        "the column did not say the folder is empty: {texts:?}"
    );
}

/// The column opens on the folder list the database cached — what the last
/// `LIST` named — rather than standing on an offered INBOX until a sync
/// runs. That list is also where the two kinds of row come from: the
/// mailboxes the server still has, and the ones it does not, which are here
/// for the mail cached under them.
#[test]
fn the_column_opens_on_the_folder_list_the_database_cached() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_folders(
        &ctx,
        &[("INBOX", true, 1), ("Sent", true, 0), ("Archive", false, 1)],
    );

    let texts = frame_texts(&ctx, &mut app);
    for name in ["INBOX", "Sent", "Archive"] {
        assert!(
            texts.iter().any(|text| text == name),
            "{name} should be in the column before any sync: {texts:?}"
        );
    }

    let nav = app.states.get_mut("acct").unwrap().nav();
    assert_eq!(names_of(&nav.listed), ["INBOX", "Sent"]);
    assert_eq!(names_of(&nav.unlisted), ["Archive"]);
    assert_eq!(nav.unread, 2, "the header counts every cached folder");
}

/// An account with nothing cached at all is still offered an INBOX:
/// clicking it is what starts the first fetch. That row is the only one the
/// column makes up — every other one comes from a folder the account holds.
#[test]
fn an_account_with_no_folders_yet_is_offered_its_inbox() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_folders(&ctx, &[]);

    assert!(frame_texts(&ctx, &mut app)
        .iter()
        .any(|text| text == "INBOX"));
    let nav = app.states.get_mut("acct").unwrap().nav();
    assert!(nav.placeholder);
    assert_eq!(names_of(&nav.listed), ["INBOX"]);
    assert!(nav.unlisted.is_empty());
}

/// A removal that fails has to undo itself on screen: the row is still in
/// the database, so the next start would load the account back either way,
/// and a click that quietly did nothing is worse than one that says so.
#[test]
fn a_failed_removal_puts_the_account_back_and_says_why() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_folders(&ctx, &[("INBOX", true, 1)]);
    let account = app.accounts[0].clone();
    // What the click does first: the account leaves the list at once, and
    // the worker's answer is what settles it.
    app.accounts.retain(|a| a.id != account.id);
    app.states.remove(&account.id);
    app.confirm_delete = None;

    app.apply_event(Event::AccountRemoveFailed {
        account: account.clone(),
        error: "database is locked".to_string(),
    });
    assert!(
        app.accounts.iter().any(|a| a.id == account.id),
        "the account did not come back"
    );
    assert_eq!(
        app.account_error.as_deref(),
        Some("Could not remove Test: database is locked")
    );

    // And the line stands where the click was: Preferences → Accounts.
    app.show_preferences = true;
    app.prefs_tab = PrefsTab::Accounts;
    // The modal is an `Area`: it lays itself out on the frame it opens on
    // and draws on the next, like the dump's Preferences pages.
    frame_texts(&ctx, &mut app);
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts
            .iter()
            .any(|text| text.contains("Could not remove Test")),
        "the Accounts page does not say why: {texts:?}"
    );
}

/// A sync that reports the mailboxes the column already shows is not a
/// change: the rows stay the ones already derived. One that reports
/// different mailboxes is, and it brings the counts with it.
#[test]
fn only_a_real_change_rebuilds_the_folder_rows() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_folders(&ctx, &[("INBOX", true, 1)]);
    run_app_frame(&ctx, &mut app, Vec::new());
    assert_eq!(
        app.states.get_mut("acct").unwrap().nav().listed[0].unread,
        1
    );

    // Mail arrives behind the rows' back. Nothing says the account changed,
    // so nothing is derived again — which is exactly what makes a sync that
    // found the same folders cheap.
    let state = app.states.get_mut("acct").unwrap();
    state.mailbox.folders[0]
        .messages
        .push(test_message("late", false));
    assert_eq!(
        state.nav().listed[0].unread,
        1,
        "the rows are the ones already derived"
    );

    // A warm sync: the same mailbox list, so still nothing to derive.
    app.apply_event(Event::Folders {
        account_id: "acct".to_string(),
        folders: vec!["INBOX".to_string()],
    });
    assert_eq!(
        app.states.get_mut("acct").unwrap().nav().listed[0].unread,
        1
    );

    // A sync that found a different list rebuilds the rows, and the count
    // it brings with it is the current one.
    app.apply_event(Event::Folders {
        account_id: "acct".to_string(),
        folders: vec!["INBOX".to_string(), "Sent".to_string()],
    });
    let nav = app.states.get_mut("acct").unwrap().nav();
    assert_eq!(names_of(&nav.listed), ["INBOX", "Sent"]);
    assert_eq!(nav.listed[0].unread, 2);
}

/// A folder the server no longer lists keeps its row, in a state of its
/// own, because the mail cached under it is the only copy of it here — and
/// it stays openable.
#[test]
fn a_folder_the_server_dropped_keeps_its_row_and_its_mail() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_folders(&ctx, &[("INBOX", true, 1), ("Archive", true, 2)]);

    // The next `LIST` does not name Archive.
    app.apply_event(Event::Folders {
        account_id: "acct".to_string(),
        folders: vec!["INBOX".to_string()],
    });
    let nav = app.states.get_mut("acct").unwrap().nav();
    assert_eq!(names_of(&nav.listed), ["INBOX"]);
    assert_eq!(names_of(&nav.unlisted), ["Archive"]);
    assert!(!nav.unlisted[0].listed);
    assert_eq!(nav.unlisted[0].unread, 2);

    // It is still drawn, and its mail is still there to open.
    assert!(frame_texts(&ctx, &mut app)
        .iter()
        .any(|text| text == "Archive"));
    assert_eq!(
        app.folder_messages_of("acct", "Archive")
            .map(<[MailMessage]>::len),
        Some(2)
    );
}

/// Collapsing the navigation slides the column out over the frames that
/// follow rather than making it vanish: the list takes its room a step at a
/// time (`Panel::show_collapsible`). The slide leaves the width the
/// splitter gave the column alone, so reopening brings that column back —
/// not the sliver the slide ended on.
#[test]
fn collapsing_the_navigation_slides_it_out() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);

    run_app_frame(&ctx, &mut app, Vec::new());
    let open = app.list_column.left();
    assert!(open > 0.0, "the navigation should be open to begin with");

    // The first frame of the collapse has the column on its way out: it has
    // moved, and it has not arrived.
    app.nav_collapsed = true;
    run_app_frame(&ctx, &mut app, Vec::new());
    let sliding = app.list_column.left();
    assert!(
        sliding > 0.0 && sliding < open,
        "the column should be part-way out, not at {sliding} pt (from {open} pt)"
    );

    // A few frames later the slide has run its course and the list reaches
    // the window's edge.
    for _ in 0..20 {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    assert_eq!(app.list_column.left(), 0.0, "the slide never finished");

    // Reopening runs the same slide backwards, and lands on the column's
    // own width rather than the width the slide was last seen at.
    app.nav_collapsed = false;
    for _ in 0..20 {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    assert_eq!(
        app.list_column.left(),
        open,
        "the column reopened at a width of its own"
    );
}

/// The navigation scrolls under the floating title bar exactly as the two
/// content columns do: the bar's height is space inside its scroll area,
/// so a scrolled column puts folders behind the glass instead of leaving
/// the bar over the panel fill.
#[test]
fn the_navigation_scrolls_under_the_floating_title_bar() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let folders: Vec<String> = (0..60).map(|index| format!("Folder {index:02}")).collect();
    let refs: Vec<(&str, bool, usize)> = folders
        .iter()
        .map(|name| (name.as_str(), true, 0))
        .collect();
    let mut app = app_with_folders(&ctx, &refs);
    run_app_frame(&ctx, &mut app, Vec::new());
    assert_eq!(app.nav_scroll, 0.0, "the column starts at its top");

    // The first row starts below the bar, at the top: its top edge is on
    // screen rather than under the glass.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let nav = nav_column(&app);
    let first = text_boxes_of(&shapes, nav, "Folder 00")
        .into_iter()
        .next()
        .expect("the first folder is drawn");
    assert!(
        first.top() >= TITLEBAR_HEIGHT,
        "the first row is not under the bar at rest: {first:?}"
    );

    // Down the column: the pointer over it, the wheel, then the frames the
    // scroll area's animation needs to come to rest.
    run_app_frame(
        &ctx,
        &mut app,
        vec![egui::Event::PointerMoved(egui::pos2(100.0, 400.0))],
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
    assert!(app.nav_scroll > 0.0, "the navigation did not scroll");
    assert_eq!(app.list_scroll, 0.0, "the list did not scroll");
    assert_eq!(app.mail_scroll, 0.0, "the pane holds no mail");

    // At least one folder's text is now in the bar's band, which is what
    // makes the bar glass over this column.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let nav = nav_column(&app);
    let under = folders.iter().any(|name| {
        text_boxes_of(&shapes, nav, name)
            .iter()
            .any(|row| row.top() < TITLEBAR_HEIGHT && row.bottom() > 0.0)
    });
    assert!(under, "no folder is drawn under the bar: {shapes:?}");
    // And the bar over it is glass: the animation's own state, read with
    // its target taken back down so the next frames do not leave it there.
    let floating = ctx.animate_bool_with_time(
        egui::Id::new("titlebar-floating"),
        false,
        TITLEBAR_FLOAT_TIME,
    );
    assert!(
        floating > 0.5,
        "the navigation's scroll did not turn the bar to glass: {floating}"
    );

    // The slide takes the column away, and with it the content under the
    // bar: the bar docks again even though the scroll area kept its offset.
    app.nav_collapsed = true;
    for _ in 0..20 {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    assert_eq!(
        app.nav_scroll, 0.0,
        "a collapsed column still holds the bar in glass"
    );
}

/// The columns give way to a narrowing window in one order, and each
/// boundary is where the plan says it is: the navigation collapses while
/// the list still has its own width, the list is capped beside the reading
/// pane's minimum, the pane becomes a popup when not even the two
/// minimums fit, and the list itself scrolls rather than narrowing past
/// its own.
#[test]
fn the_columns_give_way_in_one_order() {
    let plan = |window: f32, collapsed: bool| plan_columns(window, 200.0, 460.0, collapsed);

    // A wide window: everything as the splitters left it.
    let wide = plan(1400.0, false);
    assert!(!wide.popup);
    assert!(wide.list_max.is_none());
    assert!(!wide.list_scrolls);
    assert!(!wide.collapses_nav);

    // One point below navigation + list + the reading pane's minimum, and
    // the navigation is the column whose width the two others can have.
    assert!(!plan(940.0, false).collapses_nav);
    assert!(plan(939.0, false).collapses_nav);

    // With it gone the list keeps its own width while the window can spare
    // it: at exactly list + pane the list is not capped,
    assert!(plan(740.0, true).list_max.is_none());
    // and below that it is capped at the room beside the pane's minimum,
    assert_eq!(plan(739.0, true).list_max, Some(459.0));
    assert_eq!(plan(600.0, true).list_max, Some(320.0));
    // down to the two minimums side by side — and not one point further.
    assert!(!plan(520.0, true).popup);
    assert!(plan(519.0, true).popup);

    // The popup boundary only moves with the navigation's own room: a
    // window that cannot hold list and pane beside a navigation cannot
    // hold them as the only two columns either.
    assert!(!plan(720.0, false).popup);
    assert!(plan(719.0, false).popup);
    assert!(plan(600.0, false).popup);

    // The list itself never narrows past its minimum; it scrolls.
    assert!(!plan(240.0, true).list_scrolls);
    assert!(plan(239.0, true).list_scrolls);
}

/// The list's own splitter cannot take it past LIST_MAX_WIDTH: the reading
/// pane is the column that takes the rest, so a drag further than the cap
/// leaves the list where the cap puts it.
#[test]
fn the_list_splitter_stops_at_its_maximum() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 3);
    // A splitter drag the way egui has it on the frame after the drag: the
    // panel's stored width is what the pointer asked for.
    ctx.data_mut(|data| {
        data.insert_persisted(
            egui::Id::new("mail_list"),
            egui::containers::panel::PanelState {
                outer_rect: egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(LIST_MAX_WIDTH + 300.0, 900.0),
                ),
            },
        )
    });
    for _ in 0..2 {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    assert_eq!(app.list_width, LIST_MAX_WIDTH);
    assert!((app.list_column.width() - LIST_MAX_WIDTH).abs() < 0.5);
    assert!((1400.0 - app.list_column.right() - 600.0).abs() < 0.5);
}

/// Every row is inset by the chevron's column, a mail with nothing to open
/// included: the avatars line up down the whole list, and the mail that has
/// nothing to expand simply leaves the column empty.
#[test]
fn every_row_is_inset_by_the_chevron_column() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_threaded_mail(
        &ctx,
        vec![
            threaded_mail("3", "Re: Hello", "c@example.com", &["a@example.com"], 300),
            threaded_mail("1", "Hello", "a@example.com", &[], 100),
            threaded_mail("4", "Groceries", "d@example.com", &[], 50),
        ],
    );

    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let column = app.list_column;
    let avatars = avatars_in(&shapes, column);
    assert_eq!(
        avatars.len(),
        2,
        "a conversation's row and a mail's, one avatar each: {avatars:?}"
    );
    assert_eq!(
        avatars[0].0.x, avatars[1].0.x,
        "the two rows start at the same x: {avatars:?}"
    );
    for (_, diameter) in &avatars {
        assert_eq!(
            *diameter, AVATAR_DIAMETER,
            "a row that is not one mail of an open conversation draws the full avatar"
        );
    }
    let expected =
        column.left() + LIST_PADDING.left as f32 + THREAD_INDENT + 8.0 + AVATAR_DIAMETER / 2.0;
    assert!(
        (avatars[0].0.x - expected).abs() < 0.5,
        "the avatar is at {}, expected {expected}",
        avatars[0].0.x
    );
}

/// The navigation column's account header opens with the app's chevron
/// (`paint_chevron`) rather than a solid triangle: the last one of those in
/// the UI is gone (`docs/ui-style.md`).
#[test]
fn the_account_row_folds_its_folders_with_a_chevron() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    run_app_frame(&ctx, &mut app, Vec::new());
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);

    let nav = nav_column(&app);
    let chevrons = chevrons_in(&shapes, nav);
    assert_eq!(
        chevrons.len(),
        1,
        "the account's one disclosure is a chevron: {chevrons:?}"
    );
    // The sync cell's refresh glyph has a solid arrowhead, which is a glyph
    // rather than a disclosure. The account's disclosure is the chevron
    // above, and no other solid triangle belongs in this column.
    let sync = ctx
        .read_response(egui::Id::new(("account-sync", "acct")))
        .expect("the account row drew its sync cell")
        .rect;
    let outside_sync: Vec<Vec<egui::Pos2>> = triangles_in(&shapes, nav)
        .into_iter()
        .filter(|points| !sync.contains_rect(egui::Rect::from_points(points)))
        .collect();
    assert!(
        outside_sync.is_empty(),
        "the navigation column draws no solid disclosure triangle: {outside_sync:?}"
    );

    // A click on the row folds the folders away, and the chevron turns with
    // them: right while they are away, down while they are out.
    let down = chevrons[0][1].y > chevrons[0][0].y;
    assert!(down, "it points down while the folders are out");
    let row = egui::Rect::from_center_size(
        egui::pos2(chevrons[0][1].x, chevrons[0][1].y),
        egui::vec2(20.0, 20.0),
    );
    click_at(&ctx, &mut app, row.center());
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let folded = chevrons_in(&shapes, nav_column(&app));
    assert_eq!(folded.len(), 1, "the one disclosure is still there");
    assert!(
        folded[0][1].x > folded[0][0].x,
        "and it points right once the folders are away, where a down chevron's \
             middle point shares its ends' x: {folded:?}"
    );
}

/// Dragging the splitter between the list and the reading pane with the
/// pointer, the way a user does: the list widens, the pane narrows, and the
/// body keeps the width it settled at until the drag has stopped — which is
/// the whole fix for a drag that used to re-lay the mail out on every frame.
#[test]
fn a_splitter_drag_holds_the_body_until_the_geometry_settles() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 20);
    let html = "<p>one two three four five six seven eight nine ten eleven twelve</p>".repeat(40);
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
        .body_html = Some(html);
    app.selected_message = Some("0".to_string());

    let list_width = |ctx: &egui::Context| {
        egui::containers::panel::PanelState::load(ctx, egui::Id::new("mail_list"))
            .expect("the message list stored its width")
            .outer_rect
            .width()
    };
    let content_width = |ctx: &egui::Context| {
        let nav = panel_width(ctx, "nav").unwrap_or(NAV_DEFAULT_WIDTH);
        1400.0 - nav - list_width(ctx)
    };
    run_app_frame(&ctx, &mut app, Vec::new());
    let settled = list_width(&ctx);
    // The body is narrower than the pane by the card's gutter, its padding
    // and the scroll bar; that inset is what a reflow has to keep.
    let body_width = held_body_width(&app, "INBOX/0").expect("the body was laid out");
    let inset = content_width(&ctx) - body_width;
    assert!(inset > 0.0, "the body should be inside the pane");

    // Press on the splitter and pull it right — widening the list and
    // narrowing the pane, which is the drag that used to cost a reflow a
    // frame. The grab strip straddles the list's right edge; the reading
    // pane's own widget reaches exactly to it and is drawn later, so the
    // half that is reachable is the one just inside the list
    // (`resize_grab_radius_side` is 3 pt).
    let splitter = egui::pos2(app.list_column.right() - 1.5, 400.0);
    run_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(splitter)]);
    run_app_frame(
        &ctx,
        &mut app,
        vec![egui::Event::PointerButton {
            pos: splitter,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        }],
    );
    // egui stores a panel's width only when the drag ends (`PanelState` is
    // deliberately left at the pre-drag size), so the live edge is the one
    // the message list recorded: its right side is the pane's left.
    let pane_left = |app: &KimoApp| app.list_column.right();
    let start_left = pane_left(&app);
    let mut pos = splitter;
    for _ in 0..10 {
        pos.x += 8.0;
        run_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(pos)]);
        // The mail does not follow the pointer: it is still laid out for
        // the width it settled at, however far the pane has moved.
        assert_eq!(
            held_body_width(&app, "INBOX/0"),
            Some(body_width),
            "the body reflowed mid-drag"
        );
    }
    assert!(
        pane_left(&app) > start_left + 1.0,
        "the pane did not follow the drag"
    );
    run_app_frame(
        &ctx,
        &mut app,
        vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        }],
    );

    // The drag is over: the pane has stored the width it came to rest at,
    // and once the geometry has held still for the settle time the body
    // reflows once for it.
    let wider = list_width(&ctx);
    assert!(wider > settled, "the list did not settle wider");
    std::thread::sleep(GEOMETRY_SETTLE + Duration::from_millis(20));
    run_app_frame(&ctx, &mut app, Vec::new());
    let reflowed = held_body_width(&app, "INBOX/0").expect("the body is still laid out");
    let pane_content_width = content_width(&ctx);
    assert!(
        (reflowed - (pane_content_width - inset)).abs() < 1.0,
        "the body reflowed to {reflowed}, expected {}",
        pane_content_width - inset
    );
}

/// The Send button is enabled by one rule (`send_blocker`) — and the worker
/// checks the same one again. No account, an account with no outgoing
/// server, a sender that is not an address, an entry that is not one, and a
/// mail with nobody to go to are the reasons a draft cannot leave.
#[test]
fn the_send_button_waits_for_an_account_a_sender_and_a_recipient() {
    let can_send = |account: &Account| {
        let mut account = account.clone();
        account.smtp = Some(SmtpSettings {
            host: "smtp.example.com".to_string(),
            port: 587,
            tls: TlsMode::StartTls,
            login: None,
        });
        account
    };
    let bob = ["bob@example.com".to_string()];
    let nobody: [String; 0] = [];
    let junk = ["not an address".to_string()];

    // No account configured at all.
    assert!(send_blocker(None, &bob, &nobody, &nobody).is_some());
    // An account that only reads mail.
    let reading = test_account("imap.example.com");
    assert!(!reading.can_send());
    assert!(send_blocker(Some(&reading), &bob, &nobody, &nobody).is_some());
    // The same account with a way to send, and somebody to send to.
    let ready = can_send(&reading);
    assert_eq!(send_blocker(Some(&ready), &bob, &nobody, &nobody), None);
    // Nobody to send to.
    assert!(send_blocker(Some(&ready), &nobody, &nobody, &nobody).is_some());
    // An entry that is not an address, in any of the three fields.
    assert!(send_blocker(Some(&ready), &junk, &nobody, &nobody).is_some());
    assert!(send_blocker(Some(&ready), &bob, &junk, &nobody).is_some());
    assert!(send_blocker(Some(&ready), &bob, &nobody, &junk).is_some());
    // A username that is not an address cannot be the sender: the From
    // list shows it as one, so the card must not pretend otherwise.
    let mut odd = ready.clone();
    odd.username = "ada".to_string();
    assert!(send_blocker(Some(&odd), &bob, &nobody, &nobody).is_some());
    // The reason names what is wrong: the card shows it verbatim.
    let reason = send_blocker(Some(&ready), &junk, &nobody, &nobody).unwrap();
    assert!(reason.contains("not an address"), "{reason}");
    let unreadable = send_blocker(Some(&reading), &bob, &nobody, &nobody).unwrap();
    assert!(unreadable.contains("outgoing server"), "{unreadable}");
}

/// A message the account already holds under another folder is not news a
/// second time: a server that files one mail in two mailboxes (Gmail's All
/// Mail, a label beside the Inbox) must not raise two notifications for it.
#[test]
fn a_mail_the_account_already_holds_elsewhere_is_not_announced_again() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_folders(&ctx, &[("INBOX", true, 1), ("Receipts", true, 1)]);
    // Each fixture mail carries a Message-ID of its own, so the Inbox's
    // mail — asked about the way a batch that just landed in the Inbox
    // asks — is held nowhere else.
    let inbox_id = app.states["acct"].mailbox.folders[0].messages[0]
        .message_id
        .clone();
    assert!(!app
        .message_ids_elsewhere("acct", "INBOX")
        .contains(inbox_id.as_str()));

    // One mail filed in both — a label beside the Inbox, or Gmail's All
    // Mail — is held elsewhere whichever folder the batch came from, and it
    // is the news of the first of them that is announced.
    app.states.get_mut("acct").unwrap().mailbox.folders[1].messages[0].message_id =
        inbox_id.clone();
    assert!(app
        .message_ids_elsewhere("acct", "INBOX")
        .contains(inbox_id.as_str()));
    assert!(app
        .message_ids_elsewhere("acct", "Receipts")
        .contains(inbox_id.as_str()));

    // An id nobody holds, mail cached before the header was kept (no id at
    // all), and a mail of another account: none of them is in the set.
    let elsewhere = app.message_ids_elsewhere("acct", "INBOX");
    assert!(!elsewhere.contains("nobody@example.com"));
    assert!(!elsewhere.contains(""));
    assert!(app.message_ids_elsewhere("other", "INBOX").is_empty());
}
