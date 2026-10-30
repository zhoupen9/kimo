use super::super::*;
use super::common::*;

/// The interval the preference asks for, and the clamp it is read through:
/// nothing in settings.json — a hand-edited file included — gets a sweep
/// every frame, or one a week.
#[test]
fn the_auto_sync_interval_is_the_preference_clamped() {
    let minutes = |n: u64| Duration::from_secs(n * SECS_PER_MINUTE);
    assert_eq!(auto_sync_interval(None), minutes(AUTO_SYNC_MINUTES as u64));
    assert_eq!(auto_sync_interval(Some(0)), minutes(1));
    assert_eq!(auto_sync_interval(Some(30)), minutes(30));
    assert_eq!(auto_sync_interval(Some(100_000)), minutes(1440));
}

/// The automatic sync's clock: nothing at all while it is switched off, the
/// whole interval right after a sweep, and due — never negative — once that
/// interval has passed.
#[test]
fn the_auto_sync_waits_out_its_interval() {
    let start = Instant::now();
    let every = Duration::from_secs(600);
    assert_eq!(auto_sync_wait(false, start, every, start), None);
    assert_eq!(auto_sync_wait(true, start, every, start), Some(every));
    assert_eq!(
        auto_sync_wait(true, start, every, start + Duration::from_secs(599)),
        Some(Duration::from_secs(1))
    );
    assert_eq!(
        auto_sync_wait(true, start, every, start + every),
        Some(Duration::ZERO)
    );
    assert_eq!(
        auto_sync_wait(true, start, every, start + Duration::from_secs(90_000)),
        Some(Duration::ZERO),
        "a clock that is long overdue is due now, not in the past"
    );
}

/// An export answers with the path the worker wrote, and the desktop is told
/// where it went: the two lines a notification shows, and nothing above the
/// message list. The path is the whole of it — the name is the dialog's or the
/// writer's, and neither is known until the file is on disk. The line itself
/// is not raised here (the arm calls `crate::notify::show`, which posts to
/// the thread that talks to a real daemon), so what is pinned is what it says.
#[test]
fn a_finished_export_says_where_the_file_went() {
    let (summary, body) = exported_notification(Path::new("/home/ada/Desktop/Invoice 42.eml"));
    assert_eq!(summary, "Mail exported");
    assert_eq!(body, "Saved to /home/ada/Desktop/Invoice 42.eml");
}

/// The automatic sync is what its preference says: a sweep of every account
/// once the interval since the last one is up, and nothing before that. The
/// preference is the whole switch — the title bar's Sync, and the startup
/// sweep, are not this clock.
#[test]
fn the_automatic_sync_sweeps_when_its_interval_is_up() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    // A port with nothing behind it: the sweep reaches the job, which fails
    // to connect and says so on the account. What the clock does is the
    // question here, not what the server answers.
    app.accounts[0].host = "127.0.0.1".to_string();
    app.accounts[0].port = 1;
    app.auto_sync_every = Duration::from_secs(10 * SECS_PER_MINUTE);

    // One frame with the clock where the case wants it: what is on the
    // channel beforehand is dropped, and what arrives after is the answer.
    let frame = |app: &mut KimoApp| {
        while app.rx.try_recv().is_ok() {}
        let _ = frame_texts(&ctx, app);
    };

    // Switched off: an overdue clock sweeps nothing.
    app.auto_synced_at = Instant::now() - Duration::from_secs(3600);
    frame(&mut app);
    assert!(
        !sync_starts_within(&mut app, Duration::from_millis(300)),
        "the preference is off"
    );

    // Switched on, and not yet due: nothing either.
    app.auto_sync = true;
    app.auto_synced_at = Instant::now();
    frame(&mut app);
    assert!(
        !sync_starts_within(&mut app, Duration::from_millis(300)),
        "the interval has not passed"
    );

    // Due: every account is asked.
    app.auto_synced_at = Instant::now() - Duration::from_secs(601);
    frame(&mut app);
    assert!(
        sync_starts_within(&mut app, Duration::from_secs(5)),
        "the sweep starts once its interval is up"
    );

    // And the preference has the last word: switched off, an overdue clock
    // sweeps nothing, however long it has been overdue.
    app.auto_sync = false;
    app.auto_synced_at = Instant::now() - Duration::from_secs(3600);
    frame(&mut app);
    assert!(
        !sync_starts_within(&mut app, Duration::from_millis(300)),
        "nothing sweeps on a clock nobody asked for"
    );
}

/// The hidden half of the same clock. With the window dropped there is no
/// frame to run `pump_auto_sync`, so `pump_hidden` is what starts an overdue
/// sweep — the path `Kimo::hide_window` arms by pumping as it drops the
/// window. A hide that ran no frame (the compositor's close) used to clear
/// the only deadline the loop had and sleep through every interval.
#[test]
fn a_hidden_clock_sweeps_when_its_interval_is_up() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    // A port with nothing behind it: the sweep reaches the job, which fails
    // to connect. That it starts is the question, as in the visible test.
    app.accounts[0].host = "127.0.0.1".to_string();
    app.accounts[0].port = 1;
    app.auto_sync_every = Duration::from_secs(10 * SECS_PER_MINUTE);
    let quiet = |app: &mut KimoApp| {
        while app.rx.try_recv().is_ok() {}
    };

    // Switched on, not yet due: a hidden tick sweeps nothing.
    app.auto_sync = true;
    app.auto_synced_at = Instant::now();
    quiet(&mut app);
    app.pump_hidden();
    assert!(
        !sync_starts_within(&mut app, Duration::from_millis(300)),
        "the interval has not passed"
    );

    // Due: the sweep starts with no frame involved.
    app.auto_synced_at = Instant::now() - Duration::from_secs(601);
    quiet(&mut app);
    app.pump_hidden();
    assert!(
        sync_starts_within(&mut app, Duration::from_secs(5)),
        "a hidden clock starts its sweep once the interval is up"
    );

    // And the preference still has the last word.
    app.auto_sync = false;
    app.auto_synced_at = Instant::now() - Duration::from_secs(3600);
    quiet(&mut app);
    app.pump_hidden();
    assert!(
        !sync_starts_within(&mut app, Duration::from_millis(300)),
        "nothing sweeps on a clock nobody asked for"
    );
}

/// The title bar's Sync asks for a sweep at any time — the app syncs at
/// startup on its own, and this is the same sweep by hand. It is what the
/// cell means now: "sync now", not "the mode that needs a manual sync".
#[test]
fn the_sync_cell_asks_for_a_sweep_at_any_time() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    // A port with nothing behind it, so the job fails at connect rather
    // than reaching a server: that it *starts* is the question.
    app.accounts[0].host = "127.0.0.1".to_string();
    app.accounts[0].port = 1;
    app.selected_message = Some("0".to_string());
    run_app_frame(&ctx, &mut app, Vec::new());
    while app.rx.try_recv().is_ok() {}

    let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, TITLEBAR_HEIGHT));
    let rects = titlebar_rects(bar, 3);
    // The cell is the bar's own kind of button: it fills under the pointer
    // with the bar's rounded fill, like the cog beside it.
    let filled = fill_under_the_pointer(
        &ctx,
        &mut app,
        1400.0,
        rects.sync.center(),
        TITLEBAR_BUTTON_SIZE,
    )
    .unwrap_or_else(|| panic!("no sync cell at {:?}", rects.sync.center()));
    assert_eq!(
        filled.corner_radius,
        egui::CornerRadius::same(TITLEBAR_BUTTON_CORNER_RADIUS)
    );

    click_at(&ctx, &mut app, rects.sync.center());
    assert!(
        sync_starts_within(&mut app, Duration::from_secs(5)),
        "the sync cell starts a sweep"
    );
}

/// The sync cell on an account's own row is about that account and no
/// other: one SyncStarted, carrying the account the cell belongs to, and
/// the row stays open — the cell is not the disclosure.
#[test]
fn an_accounts_sync_cell_syncs_only_that_account() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_two_accounts(&ctx, 1);
    // A port with nothing behind it: the job fails at connect, and that it
    // starts for this account is the question.
    for account in &mut app.accounts {
        account.host = "127.0.0.1".to_string();
        account.port = 1;
    }
    run_app_frame(&ctx, &mut app, Vec::new());
    let sync = ctx
        .read_response(egui::Id::new(("account-sync", "other")))
        .expect("the second account's row drew a sync cell");
    assert!(sync.rect.width() > 0.0);
    while app.rx.try_recv().is_ok() {}

    // The cell is a hover control: no glyph at rest, the refresh arrow
    // under the pointer.
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(
        triangles_in(&shapes, sync.rect).is_empty(),
        "the sync glyph is visible without the pointer"
    );
    run_app_frame(
        &ctx,
        &mut app,
        vec![egui::Event::PointerMoved(sync.rect.center())],
    );
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(
        !triangles_in(&shapes, sync.rect).is_empty(),
        "hovering the row did not reveal the sync glyph"
    );

    click_at(&ctx, &mut app, sync.rect.center());
    let mut started = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match app.rx.try_recv() {
            Ok(Event::SyncStarted { account_id }) => {
                started = Some(account_id);
                break;
            }
            Ok(_) => {}
            Err(mpsc::TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(2)),
            Err(mpsc::TryRecvError::Disconnected) => break,
        }
    }
    assert_eq!(
        started.as_deref(),
        Some("other"),
        "the cell asked for its own account only"
    );
    assert!(
        !app.collapsed_accounts.contains("other"),
        "the sync cell is not the row's disclosure"
    );

    // While that account is syncing the cell is still a target: its click
    // is swallowed by the guard, not by the row's disclosure.
    app.states
        .get_mut("other")
        .expect("the second account")
        .syncing = true;
    run_app_frame(&ctx, &mut app, Vec::new());
    click_at(&ctx, &mut app, sync.rect.center());
    assert!(
        !app.collapsed_accounts.contains("other"),
        "a click on the spinner folded the account"
    );
}

/// The tray's badge is the Inbox's own state and nothing else: it is up
/// while any account's Inbox holds mail nobody has read, it goes the moment
/// that mail is read, and no other folder — and no window state — has a say.
#[test]
fn the_tray_badge_follows_unread_mail_in_the_inbox() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_two_accounts(&ctx, 1);

    // The fixture's mail is unread: the badge is up before any frame, from
    // the state the database hands a session at startup.
    app.update_tray_badge();
    assert!(app.tray_badge, "an Inbox with unread mail wears the badge");

    // Read everywhere: the Inbox has nothing left to say.
    for state in app.states.values_mut() {
        for mail in state
            .mailbox
            .folders
            .iter_mut()
            .flat_map(|folder| folder.messages.iter_mut())
        {
            mail.seen = true;
        }
        state.folders_changed();
    }
    app.update_tray_badge();
    assert!(!app.tray_badge, "a read Inbox is not news");

    // Unread mail in another folder is not the Inbox's badge.
    let state = app.states.get_mut("acct").expect("the fixture's account");
    state.mailbox.folders.push(storage::Folder {
        name: "Archive".to_string(),
        uidvalidity: Some(1),
        synced: true,
        listed: true,
        can_load_older: false,
        messages: vec![test_message("archived", false)],
    });
    state.folders.push("Archive".to_string());
    state.folders_changed();
    app.update_tray_badge();
    assert!(!app.tray_badge, "only the Inbox wears it");

    // And one account's unread Inbox is the whole mailbox's badge, however
    // far from the account on screen it is.
    let other = app.states.get_mut("other").expect("the second account");
    other.mailbox.folders[0].messages[0].seen = false;
    other.folders_changed();
    app.update_tray_badge();
    assert!(app.tray_badge, "every account's Inbox speaks for the badge");
}

/// The mail actions are one group and their order is the bar's: Export
/// stands immediately before Delete — the mail is written out before it is
/// thrown away — and every action says what it is, the two wired ones
/// saying what they do.
#[test]
fn export_stands_immediately_before_delete() {
    assert_eq!(
        MailAction::ALL,
        [
            MailAction::Reply,
            MailAction::ReplyAll,
            MailAction::Forward,
            MailAction::Label,
            MailAction::Flag,
            MailAction::Export,
            MailAction::Delete,
        ]
    );
    for action in MailAction::ALL {
        assert!(!action.label().is_empty());
        assert!(!action.tooltip().is_empty());
    }
    // One says where the file goes, the other that it cannot be undone.
    assert!(MailAction::Export.tooltip().contains(".eml"));
    assert!(MailAction::Delete.tooltip().contains("server"));
}

/// A click on the close cell asks the viewport to close when close-to-tray
/// is off; the tray path is `close_command`'s above.
#[test]
fn the_close_cell_closes_when_close_to_tray_is_off() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    // One frame first: the right end is carved out of the window's size.
    frame_shapes_at(&ctx, &mut app, 1400.0);
    app.close_to_tray = false;

    let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, TITLEBAR_HEIGHT));
    let rects = titlebar_rects(bar, 3);
    let center = egui::pos2(
        rects.controls.right() - TITLEBAR_BUTTON_SIZE / 2.0,
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
        commands.contains(&egui::ViewportCommand::Close),
        "the close cell asks for the normal close: {commands:?}"
    );
}

/// What the notice says about a mail that went out: who it went to, and
/// what became of the copy in Sent — with the mailbox name decoded for the
/// reader rather than left in the wire's modified UTF-7.
#[test]
fn a_sent_notice_names_the_recipients_and_the_copy() {
    let recipients = [
        "bob@example.com".to_string(),
        "carol@example.com".to_string(),
    ];
    assert_eq!(
        sent_notice(&recipients, &SentCopy::None),
        "Sent to bob@example.com, carol@example.com"
    );
    assert_eq!(
        sent_notice(
            &recipients,
            &SentCopy::Saved {
                mailbox: "&XfJT0ZAB-".to_string(),
            }
        ),
        "Sent to bob@example.com, carol@example.com · a copy is in 已发送"
    );
    let failed = sent_notice(
        &recipients,
        &SentCopy::Failed("no such mailbox".to_string()),
    );
    assert!(
        failed.ends_with("the copy could not be saved: no such mailbox"),
        "{failed}"
    );
}

/// A batch of news is one notification per mail while there are few of
/// them, and one line counting them once there are many: forty popups is
/// not something a desktop should be asked to draw.
#[test]
fn a_batch_of_new_mail_becomes_one_notification_per_mail_or_a_count() {
    let arrivals = |count: usize| -> Vec<Arrival> {
        (0..count)
            .map(|index| Arrival::of(&test_message(&index.to_string(), false)))
            .collect()
    };

    let notices = new_mail_notices("INBOX", &arrivals(NOTIFY_MAX));
    assert_eq!(notices.len(), NOTIFY_MAX);
    assert_eq!(
        notices[0],
        ("Ada <ada@example.com>".to_string(), "Subject 0".to_string())
    );

    let notices = new_mail_notices("INBOX", &arrivals(NOTIFY_MAX + 1));
    assert_eq!(notices.len(), 1, "one summary instead of four popups");
    assert_eq!(notices[0].0, "4 new messages in INBOX");
    assert_eq!(notices[0].1.lines().count(), NOTIFY_MAX);
    assert!(notices[0].1.starts_with("Ada <ada@example.com>: Subject 0"));
}
