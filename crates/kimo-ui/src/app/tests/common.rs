use super::super::*;

/// One headless frame of the real app at a given list-splitter position,
/// timed the way the integration times one: egui's own pass plus the
/// tessellation the renderer would do.
pub(super) fn probe_frame(
    ctx: &egui::Context,
    app: &mut KimoApp,
    list: f32,
    scale: f32,
) -> Duration {
    // Where the drag has the splitter: egui keeps a side panel's width as
    // its `PanelState`, and the next frame reads it back — which is
    // exactly what a drag frame updates.
    ctx.data_mut(|d| {
        d.insert_persisted(
            egui::Id::new("mail_list"),
            egui::containers::panel::PanelState {
                outer_rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(list, 900.0)),
            },
        )
    });
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1400.0, 900.0),
        )),
        ..Default::default()
    };
    let start = Instant::now();
    let mut output = ctx.run_ui(input, |ui| app.ui(ui));
    let shapes = std::mem::take(&mut output.shapes);
    let _ = ctx.tessellate(shapes, scale);
    output.textures_delta.clear();
    start.elapsed()
}

pub(super) fn probe_median(times: &mut [Duration]) -> Duration {
    times.sort();
    times[times.len() / 2]
}

pub(super) fn probe_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// The real database, if this machine has one: the probe drives the app on
/// the mail actually cached here. Read-only; no workers are started.
pub(super) fn probe_app(ctx: &egui::Context) -> Option<KimoApp> {
    let accounts = crate::db::load_accounts().ok()?;
    if accounts.is_empty() {
        return None;
    }
    let ids: Vec<String> = accounts.iter().map(|a| a.id.clone()).collect();
    let mailboxes = crate::db::load_mailboxes(&ids).unwrap_or_default();
    let settings = storage::load_settings();
    let mut app = KimoApp::assemble(
        ctx,
        accounts,
        mailboxes,
        &settings,
        std::sync::Arc::new(crate::plugin::discovery::Registry::empty()),
    );
    // The folder with the most mail, and the largest HTML body in it: the
    // worst case a splitter drag can be asked to re-lay out.
    let (account_id, folder) = app
        .states
        .iter()
        .flat_map(|(id, state)| {
            state
                .mailbox
                .folders
                .iter()
                .map(move |folder| (id.clone(), folder))
        })
        .max_by_key(|(_, folder)| folder.messages.len())
        .map(|(id, folder)| (id, folder.name.clone()))?;
    let uid = app
        .states
        .get(&account_id)?
        .mailbox
        .folders
        .iter()
        .find(|folder_| folder_.name == folder)?
        .messages
        .iter()
        .max_by_key(|msg| msg.body_html.as_ref().map_or(0, String::len))?
        .uid
        .clone();
    app.selected_account = Some(account_id);
    app.selected_folder = Some(folder);
    app.selected_message = Some(uid);
    Some(app)
}

/// One cached mail, as the fixtures build them: the subject follows the
/// uid and `seen` is whatever the test needs of it.
pub(super) fn test_message(uid: &str, seen: bool) -> MailMessage {
    MailMessage {
        uid: uid.to_string(),
        subject: format!("Subject {uid}"),
        from: "Ada <ada@example.com>".to_string(),
        to: "me@example.com".to_string(),
        date: "2026-01-01".to_string(),
        timestamp: Some(1_700_000_000),
        seen,
        flagged: false,
        message_id: format!("{uid}@example.com"),
        in_reply_to: Vec::new(),
        references: Vec::new(),
        body_text: "body".to_string(),
        body_html: None,
        preview: "preview".to_string(),
        attachments: Vec::new(),
        inline_images: Vec::new(),
        labels: BTreeSet::new(),
    }
}

/// An app with `count` mails in one folder: no database, no workers, and
/// nothing to sync. The frame tests below drive it headlessly.
pub(super) fn app_with_messages(ctx: &egui::Context, count: usize) -> KimoApp {
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
    let messages = (0..count)
        .map(|index| test_message(&index.to_string(), false))
        .collect();
    let folder = storage::Folder {
        name: "INBOX".to_string(),
        uidvalidity: Some(1),
        synced: true,
        listed: true,
        can_load_older: false,
        messages,
    };
    let mut mailboxes = HashMap::new();
    mailboxes.insert(
        account.id.clone(),
        storage::Mailbox {
            folders: vec![folder],
        },
    );
    let mut app = KimoApp::assemble(
        ctx,
        vec![account],
        mailboxes,
        &storage::Settings::default(),
        std::sync::Arc::new(crate::plugin::discovery::Registry::empty()),
    );
    // The fixtures below measure geometry, and the warning the app carries
    // when *this machine's* database cannot be written stands at the top of
    // the middle column, over the first row. It is a fact about the machine,
    // not about the layout the test is pinning — and so are the labels the
    // machine's database holds, which the app loads at startup.
    app.storage_warning = None;
    app.labels.clear();
    app.label_view = None;
    app
}

/// An app whose one account holds the folders given as
/// `(name, listed, mails)` — the shape the database hands `assemble` on
/// startup, unread mail and all. The navigation tests drive it.
pub(super) fn app_with_folders(ctx: &egui::Context, folders: &[(&str, bool, usize)]) -> KimoApp {
    let mut app = app_with_messages(ctx, 0);
    let state = app.states.get_mut("acct").unwrap();
    state.mailbox.folders = folders
        .iter()
        .map(|(name, listed, count)| storage::Folder {
            name: name.to_string(),
            uidvalidity: Some(1),
            synced: *count > 0,
            listed: *listed,
            can_load_older: false,
            messages: (0..*count)
                .map(|index| test_message(&format!("{name}-{index}"), false))
                .collect(),
        })
        .collect();
    // The column opens on the mailboxes the last `LIST` named, which is how
    // `assemble` fills this in.
    state.folders = state
        .mailbox
        .folders
        .iter()
        .filter(|folder| folder.listed)
        .map(|folder| folder.name.clone())
        .collect();
    state.folders_changed();
    app
}

/// A scan of two cards and the four manifests a Mesa + NVIDIA installation
/// carries — the Graphics page's input, without reading this machine's
/// sysfs in a test.
pub(super) fn gpu_scan_fixture() -> crate::gpus::Scan {
    let cards = vec![
        crate::gpus::Card {
            pci: "0000:03:00.0".to_string(),
            vendor: 0x8086,
            device: 0xe20b,
            driver: "xe".to_string(),
        },
        crate::gpus::Card {
            pci: "0000:0b:00.0".to_string(),
            vendor: 0x10de,
            device: 0x1e84,
            driver: "nvidia".to_string(),
        },
    ];
    let names = crate::gpus::names_from(
        concat!(
            "8086  Intel Corporation\n",
            "\te20b  Battlemage G21 [Arc B580]\n",
            "10de  NVIDIA Corporation\n",
            "\t1e84  TU104 [GeForce RTX 2070 SUPER]\n",
        ),
        &cards,
    );
    let icd = |file: &str, vendor: Option<u32>, software: bool| crate::gpus::Icd {
        path: std::path::PathBuf::from(format!("/usr/share/vulkan/icd.d/{file}")),
        library: file.to_string(),
        vendor,
        software,
    };
    crate::gpus::Scan {
        cards,
        icds: vec![
            icd("intel_hasvk_icd.json", Some(0x8086), false),
            icd("intel_icd.json", Some(0x8086), false),
            icd("lvp_icd.json", None, true),
            icd("nvidia_icd.json", Some(0x10de), false),
        ],
        names,
        locked: false,
    }
}

/// The wire names of a group of rows, for the assertions about which
/// folders are in which.
pub(super) fn names_of(rows: &[FolderRow]) -> Vec<&str> {
    rows.iter().map(|row| row.name.as_str()).collect()
}

/// The width the built-in renderer holds one mail's body at, for the tests
/// that read the layout the pane settled on — `None` when it holds none.
pub(super) fn held_body_width(app: &KimoApp, identity: &str) -> Option<f32> {
    app.body
        .entries
        .iter()
        .find(|entry| entry.identity == identity)
        .and_then(|entry| entry.width)
}

/// Every run of text one headless frame of the app drew, for the tests that
/// care what a column says.
pub(super) fn frame_texts(ctx: &egui::Context, app: &mut KimoApp) -> Vec<String> {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1400.0, 900.0),
        )),
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| app.ui(ui));
    output.textures_delta.clear();
    let mut texts = Vec::new();
    for clipped in &output.shapes {
        collect_text(&clipped.shape, &mut texts);
    }
    texts
}

pub(super) fn collect_text(shape: &egui::Shape, out: &mut Vec<String>) {
    match shape {
        egui::Shape::Text(text) => out.push(text.galley.text().to_string()),
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_text(shape, out);
            }
        }
        _ => {}
    }
}

/// Whether a shape samples a texture — the About page's way of checking
/// that its icon was actually drawn, not merely uploaded.
pub(super) fn draws_texture(shape: &egui::Shape, id: egui::TextureId) -> bool {
    match shape {
        egui::Shape::Mesh(mesh) => mesh.texture_id == id,
        egui::Shape::Vec(shapes) => shapes.iter().any(|shape| draws_texture(shape, id)),
        _ => false,
    }
}

/// Every run of text one headless frame drew with the color it was laid out
/// in, for the tests that care how something reads and not only what it
/// says. The color a run is drawn in is the one its galley was laid out
/// with; the color handed to `Painter::galley` is only the fallback.
pub(super) fn frame_text_colors(
    ctx: &egui::Context,
    app: &mut KimoApp,
) -> Vec<(String, egui::Color32)> {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1400.0, 900.0),
        )),
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| app.ui(ui));
    output.textures_delta.clear();
    let mut texts = Vec::new();
    for clipped in &output.shapes {
        collect_text_color(&clipped.shape, &mut texts);
    }
    texts
}

/// The boxes of every run of text a frame drew inside `rect` — a galley's own
/// rect, which is what a row places its lines by, not the ink drawn on it.
pub(super) fn text_boxes_in(
    shapes: &[egui::epaint::ClippedShape],
    rect: egui::Rect,
) -> Vec<egui::Rect> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_text_boxes(&clipped.shape, rect, &mut out);
    }
    out
}

pub(super) fn collect_text_boxes(shape: &egui::Shape, rect: egui::Rect, out: &mut Vec<egui::Rect>) {
    match shape {
        egui::Shape::Text(text) => {
            let drawn = egui::Rect::from_min_size(text.pos, text.galley.size());
            if rect.contains_rect(drawn) {
                out.push(drawn);
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_text_boxes(shape, rect, out);
            }
        }
        _ => {}
    }
}

pub(super) fn collect_text_color(shape: &egui::Shape, out: &mut Vec<(String, egui::Color32)>) {
    match shape {
        egui::Shape::Text(text) => {
            let color = text
                .galley
                .job
                .sections
                .first()
                .map_or(text.fallback_color, |section| section.format.color);
            out.push((text.galley.text().to_string(), color));
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_text_color(shape, out);
            }
        }
        _ => {}
    }
}

/// Every rectangle one headless frame painted, for the tests that click
/// something the app drew rather than a position worked out by arithmetic.
pub(super) fn frame_rects(ctx: &egui::Context, app: &mut KimoApp) -> Vec<egui::Rect> {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1400.0, 900.0),
        )),
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| app.ui(ui));
    output.textures_delta.clear();
    let mut rects = Vec::new();
    for clipped in &output.shapes {
        collect_rects(&clipped.shape, &mut rects);
    }
    rects
}

pub(super) fn collect_rects(shape: &egui::Shape, out: &mut Vec<egui::Rect>) {
    match shape {
        egui::Shape::Rect(rect) => out.push(rect.rect),
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_rects(shape, out);
            }
        }
        _ => {}
    }
}

/// One headless frame of an app under test, with the given input events.
pub(super) fn run_app_frame(ctx: &egui::Context, app: &mut KimoApp, events: Vec<egui::Event>) {
    run_app_frame_sized(ctx, app, events, egui::vec2(1400.0, 900.0));
}

/// The same frame, in a window of the size given — what the layout tests
/// below resize the window with.
pub(super) fn run_app_frame_sized(
    ctx: &egui::Context,
    app: &mut KimoApp,
    events: Vec<egui::Event>,
    size: egui::Vec2,
) {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        events,
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| app.ui(ui));
    output.textures_delta.clear();
}

/// One headless frame's whole output — the viewport commands and the cursor
/// icon included, which is what the window-chrome tests read: a resize grip
/// asks the compositor for a resize rather than drawing anything.
pub(super) fn app_frame_output(
    ctx: &egui::Context,
    app: &mut KimoApp,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1400.0, 900.0),
        )),
        events,
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| app.ui(ui));
    output.textures_delta.clear();
    output
}

/// What one frame asked its viewport for.
pub(super) fn viewport_commands(output: &egui::FullOutput) -> Vec<egui::ViewportCommand> {
    output
        .viewport_output
        .get(&egui::ViewportId::ROOT)
        .map(|viewport| viewport.commands.clone())
        .unwrap_or_default()
}

/// The attachment cards a frame drew, in the order they were laid out: the
/// *filled* rects whose height is a card's — a card is a fill and a stroke,
/// and the stroke is a rect of its own — in the reading pane and not in the
/// column beside it, where a height of their own says they are cards.
pub(super) fn attachment_cards_in(ctx: &egui::Context, app: &mut KimoApp) -> Vec<egui::Rect> {
    let pane_left = app.list_column.right();
    fills_in_color(&frame_shapes_at(ctx, app, 1400.0), theme::of(ctx).card)
        .into_iter()
        .filter(|rect| {
            (rect.height() - ATTACHMENT_CARD_HEIGHT).abs() < 0.5 && rect.center().x > pane_left
        })
        .collect()
}
/// Whether a sync starts within `window`: the worker sends `SyncStarted`
/// before it touches the network, so a sweep the app asked for shows up on
/// the channel at once. Whatever is already on the channel is dropped first,
/// so a sweep of an earlier frame cannot answer for this one — the caller
/// drains before the frame it is asking about, never after.
pub(super) fn sync_starts_within(app: &mut KimoApp, window: Duration) -> bool {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        match app.rx.try_recv() {
            Ok(Event::SyncStarted { .. }) => return true,
            Ok(_) => {}
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return false,
        }
    }
    false
}

/// Every vertical line a frame drew inside the list column, away from the
/// column's own sides — where the rows' hairlines run from the top of the
/// list to the bottom of it. The guide line the mails of a thread used to
/// draw down the chevron's column was one of these.
pub(super) fn vertical_lines_in(
    shapes: &[egui::epaint::ClippedShape],
    column: egui::Rect,
) -> Vec<egui::Rect> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_vertical_lines(&clipped.shape, column, &mut out);
    }
    out
}

pub(super) fn collect_vertical_lines(
    shape: &egui::Shape,
    column: egui::Rect,
    out: &mut Vec<egui::Rect>,
) {
    match shape {
        egui::Shape::LineSegment { points, .. } => {
            let bounds = egui::Rect::from_two_pos(points[0], points[1]);
            if bounds.width() < 0.6
                && bounds.height() > MESSAGE_ROW_HEIGHT / 2.0
                && bounds.center().x > column.left() + 2.0
                && bounds.center().x < column.right() - 2.0
            {
                out.push(bounds);
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_vertical_lines(shape, column, out);
            }
        }
        _ => {}
    }
}

/// The avatars a frame drew in the message list — each with its diameter,
/// since a mail of an open conversation draws a smaller one — the filled
/// circles the rows put their sender's initial on.
pub(super) fn avatars_in(
    shapes: &[egui::epaint::ClippedShape],
    column: egui::Rect,
) -> Vec<(egui::Pos2, f32)> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_avatars(&clipped.shape, column, &mut out);
    }
    out
}

pub(super) fn collect_avatars(
    shape: &egui::Shape,
    column: egui::Rect,
    out: &mut Vec<(egui::Pos2, f32)>,
) {
    match shape {
        egui::Shape::Circle(circle) => {
            let diameter = circle.radius * 2.0;
            let drawn = [AVATAR_DIAMETER, THREAD_AVATAR_DIAMETER]
                .into_iter()
                .any(|size| (diameter - size).abs() < 0.5);
            if drawn && circle.center.y > TITLEBAR_HEIGHT && column.contains(circle.center) {
                out.push((circle.center, diameter));
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_avatars(shape, column, out);
            }
        }
        _ => {}
    }
}

/// One mail of a folder the threading tests hand to the list: the headers a
/// conversation is stitched from are arguments, and the folder's own order
/// is newest first, which is what the timestamp says.
pub(super) fn threaded_mail(
    uid: &str,
    subject: &str,
    id: &str,
    references: &[&str],
    ts: i64,
) -> MailMessage {
    let mut mail = test_message(uid, false);
    mail.subject = subject.to_string();
    mail.message_id = id.to_string();
    mail.in_reply_to = references
        .iter()
        .rev()
        .take(1)
        .map(|id| id.to_string())
        .collect();
    mail.references = references.iter().map(|id| id.to_string()).collect();
    mail.timestamp = Some(ts);
    mail.date = String::new();
    mail.body_text = format!("body of {uid}");
    // The preview and the body differ on purpose: the list shows the first
    // and the reading pane the second, and a test that reads the frame
    // cannot tell them apart otherwise.
    mail.preview = format!("preview of {uid}");
    mail
}

/// A folder holding exactly these mails, with the pane on it.
pub(super) fn app_with_threaded_mail(ctx: &egui::Context, mut mails: Vec<MailMessage>) -> KimoApp {
    mails.sort_by_key(|mail| std::cmp::Reverse(mail.timestamp));
    let mut app = app_with_messages(ctx, 0);
    let state = app.states.get_mut("acct").expect("the fixture account");
    state.mailbox.folders[0].messages = mails;
    state.folders_changed();
    app.selected_account = Some("acct".to_string());
    app.selected_folder = Some("INBOX".to_string());
    app
}

/// Click at a position the way a user does: the pointer moves there, then
/// the button goes down and up.
pub(super) fn click_at(ctx: &egui::Context, app: &mut KimoApp, pos: egui::Pos2) {
    click_at_sized(ctx, app, pos, 1400.0);
}

/// A primary click at a position, with the frame's whole output: the
/// clipboard a menu row writes is a command, not a shape.
pub(super) fn click_at_output(
    ctx: &egui::Context,
    app: &mut KimoApp,
    pos: egui::Pos2,
) -> egui::FullOutput {
    let _ = app_frame_output(ctx, app, vec![egui::Event::PointerMoved(pos)]);
    let pressed = app_frame_output(
        ctx,
        app,
        vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        }],
    );
    let released = app_frame_output(
        ctx,
        app,
        vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        }],
    );
    // A context-menu row that has to copy a label selection acts on the
    // press (the click is what clears it); everything else acts on the
    // release. The caller wants whichever frame carried the command.
    if pressed
        .platform_output
        .commands
        .iter()
        .any(|command| matches!(command, egui::OutputCommand::CopyText(_)))
    {
        pressed
    } else {
        released
    }
}

/// A right-click at a position: the pointer moves there, then the
/// secondary button goes down and up. What opens a context menu.
pub(super) fn right_click_at(ctx: &egui::Context, app: &mut KimoApp, pos: egui::Pos2) {
    run_app_frame(ctx, app, vec![egui::Event::PointerMoved(pos)]);
    for pressed in [true, false] {
        run_app_frame(
            ctx,
            app,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Secondary,
                pressed,
                modifiers: egui::Modifiers::default(),
            }],
        );
    }
}

/// A click in a window of the width given: the popup layout's tests click
/// in a window the column layout never sees.
pub(super) fn click_at_sized(ctx: &egui::Context, app: &mut KimoApp, pos: egui::Pos2, width: f32) {
    run_app_frame_sized(
        ctx,
        app,
        vec![egui::Event::PointerMoved(pos)],
        egui::vec2(width, 900.0),
    );
    for pressed in [true, false] {
        run_app_frame_sized(
            ctx,
            app,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            }],
            egui::vec2(width, 900.0),
        );
    }
}

/// A conversation is one row showing its newest mail under the thread's
/// subject, with the count of what it holds — and the mails inside it get no
/// rows of their own until the chevron is clicked.
pub(super) fn a_conversation_row(app: &mut KimoApp, ctx: &egui::Context) -> egui::Pos2 {
    let texts = frame_texts(ctx, app);
    assert_eq!(
        texts.iter().filter(|text| *text == "Hello").count(),
        1,
        "one row, under the thread's own subject: {texts:?}"
    );
    assert!(
        !texts.iter().any(|text| text == "Re: Hello"),
        "the mail it holds has no row of its own: {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text == "2"),
        "the row says how many mails it holds: {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text == "preview of 3"),
        "and shows what the newest one says: {texts:?}"
    );
    egui::pos2(300.0, TITLEBAR_HEIGHT + MESSAGE_ROW_HEIGHT / 2.0)
}

/// Two read mails of one conversation, and an app showing their folder. Read
/// so the row draws each line once: an unread one is struck through twice
/// (`paint_row_text`), which is another test's subject.
pub(super) fn app_with_one_conversation(ctx: &egui::Context) -> KimoApp {
    let mut newest = threaded_mail("3", "Re: Hello", "c@example.com", &["a@example.com"], 300);
    newest.seen = true;
    let mut oldest = threaded_mail("1", "Hello", "a@example.com", &[], 100);
    oldest.seen = true;
    app_with_threaded_mail(ctx, vec![newest, oldest])
}

/// Where a row's checkbox sits: the avatar's slot, `row` rows down the
/// list. A mail of an open conversation is a step further in with a smaller
/// circle, and the box follows the slot the circle is in. Select mode's
/// select-all row takes the top of the list, so every mail row sits one
/// band lower while the mode is on.
pub(super) fn checkbox_center(app: &KimoApp, row: usize, in_thread: bool) -> egui::Pos2 {
    let (indent, diameter) = if in_thread {
        (THREAD_INDENT + THREAD_STEP, THREAD_AVATAR_DIAMETER)
    } else {
        (THREAD_INDENT, AVATAR_DIAMETER)
    };
    let select_all = if app.select_mode {
        SELECT_ALL_HEIGHT
    } else {
        0.0
    };
    egui::pos2(
        app.list_column.left() + LIST_MARGIN as f32 + indent + 8.0 + diameter / 2.0,
        TITLEBAR_HEIGHT + select_all + MESSAGE_ROW_HEIGHT * (row as f32 + 0.5),
    )
}

/// Where a row's own flag button sits: the cell just inside the delete one,
/// so the pair is what the pointer finds at the row's right end. Select
/// mode's select-all row pushes every mail row one band lower.
pub(super) fn row_flag_center(app: &KimoApp, row: usize) -> egui::Pos2 {
    row_delete_center(app, row) - egui::vec2(ROW_ACTION_SIZE + ROW_ACTION_GAP, 0.0)
}

/// Where a row's own delete button sits: the cell at the right end of the
/// row's first line, in the timestamp's slot. Select mode's select-all row
/// pushes every mail row one band lower.
pub(super) fn row_delete_center(app: &KimoApp, row: usize) -> egui::Pos2 {
    let select_all = if app.select_mode {
        SELECT_ALL_HEIGHT
    } else {
        0.0
    };
    egui::pos2(
        app.list_column.right() - LIST_MARGIN as f32 - 10.0 - ROW_ACTION_SIZE / 2.0,
        TITLEBAR_HEIGHT + select_all + MESSAGE_ROW_HEIGHT * row as f32 + ROW_SENDER_Y,
    )
}

/// Where the select-all row's own checkbox sits: the plain-row slot in the
/// band above the first row.
pub(super) fn select_all_center(app: &KimoApp) -> egui::Pos2 {
    egui::pos2(
        checkbox_center(app, 0, false).x,
        TITLEBAR_HEIGHT + SELECT_ALL_HEIGHT / 2.0,
    )
}

/// The boxes a frame drew in the message list's rows, top to bottom — the
/// select-all row's own box, if it is there, left out.
pub(super) fn row_boxes_in(
    shapes: &[egui::epaint::ClippedShape],
    column: egui::Rect,
) -> Vec<egui::Rect> {
    let mut boxes: Vec<egui::Rect> = checkboxes_in(shapes, column)
        .into_iter()
        .filter(|rect| rect.bottom() > TITLEBAR_HEIGHT + SELECT_ALL_HEIGHT)
        .collect();
    boxes.sort_by(|a, b| a.top().total_cmp(&b.top()));
    boxes
}

/// The select-all row's own box, if the frame drew the band: the box the
/// band puts in the rows' column, in the band's own slot under the title
/// bar. The band is drawn after the rows and never scrolls, so a box a row
/// happens to land on that line is the one before it — the last match is the
/// band's.
pub(super) fn select_all_box_in(
    shapes: &[egui::epaint::ClippedShape],
    column: egui::Rect,
) -> Option<egui::Rect> {
    let slot = TITLEBAR_HEIGHT + SELECT_ALL_HEIGHT / 2.0;
    checkboxes_in(shapes, column)
        .into_iter()
        .rfind(|rect| (rect.center().y - slot).abs() < 1.0)
}

/// The checkboxes a frame drew in the message list, top to bottom: a
/// `CHECKBOX_SIZE` square with the box's corner radius. Nothing else in the
/// list is that shape — the hover fill behind a box is wider than the box,
/// and a row's own edges are hairlines across the whole column.
pub(super) fn checkboxes_in(
    shapes: &[egui::epaint::ClippedShape],
    column: egui::Rect,
) -> Vec<egui::Rect> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_checkboxes(&clipped.shape, column, &mut out);
    }
    out
}

pub(super) fn collect_checkboxes(
    shape: &egui::Shape,
    column: egui::Rect,
    out: &mut Vec<egui::Rect>,
) {
    match shape {
        egui::Shape::Rect(rect) => {
            let size = rect.rect.size();
            if rect.corner_radius == egui::CornerRadius::same(CHECKBOX_CORNER_RADIUS)
                && (size.x - CHECKBOX_SIZE).abs() < 0.5
                && (size.y - CHECKBOX_SIZE).abs() < 0.5
                && column.contains_rect(rect.rect)
            {
                out.push(rect.rect);
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_checkboxes(shape, column, out);
            }
        }
        _ => {}
    }
}

/// The marks a frame drew inside a box, as the number of points each has: the
/// check on a checked box is one line of three points, and the dash of a
/// partly checked conversation a single two-point segment.
pub(super) fn strokes_in(shapes: &[egui::epaint::ClippedShape], rect: egui::Rect) -> Vec<usize> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_strokes(&clipped.shape, rect, &mut out);
    }
    out
}

pub(super) fn collect_strokes(shape: &egui::Shape, rect: egui::Rect, out: &mut Vec<usize>) {
    match shape {
        egui::Shape::LineSegment { points, .. } => {
            let bounds = egui::Rect::from_two_pos(points[0], points[1]);
            if rect.contains_rect(bounds) {
                out.push(2);
            }
        }
        egui::Shape::Path(path) if path.fill == egui::Color32::TRANSPARENT => {
            let bounds = egui::Rect::from_points(&path.points);
            if rect.contains_rect(bounds) {
                out.push(path.points.len());
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_strokes(shape, rect, out);
            }
        }
        _ => {}
    }
}

/// One headless frame of an app under test at a given window width, with
/// the shapes it drew — each with the clip rect it was drawn through, which
/// is what the title-bar test below reads.
pub(super) fn frame_shapes_at(
    ctx: &egui::Context,
    app: &mut KimoApp,
    width: f32,
) -> Vec<egui::epaint::ClippedShape> {
    frame_shapes_with(ctx, app, width, Vec::new())
}

/// `frame_shapes_at` with input events — the pointer moved over a cell,
/// say — for the tests that read what a hover drew.
pub(super) fn frame_shapes_with(
    ctx: &egui::Context,
    app: &mut KimoApp,
    width: f32,
    events: Vec<egui::Event>,
) -> Vec<egui::epaint::ClippedShape> {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(width, 900.0),
        )),
        events,
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| app.ui(ui));
    output.textures_delta.clear();
    output.shapes
}

/// The box one painted stroke covers, for the tests that look for a glyph
/// rather than for text. `None` for everything a glyph is not: text, fills,
/// images, meshes, and nested shape lists, which the caller walks itself.
pub(super) fn stroke_bounds(shape: &egui::Shape) -> Option<egui::Rect> {
    match shape {
        egui::Shape::LineSegment { points, .. } => {
            Some(egui::Rect::from_two_pos(points[0], points[1]))
        }
        egui::Shape::QuadraticBezier(quad) => Some(egui::Rect::from_points(&quad.points)),
        // A path only counts when it is a line drawing: the chevron that
        // opens a conversation is a filled polygon, and it is no more a
        // glyph than the avatar's circle is.
        egui::Shape::Path(path) if path.fill == egui::Color32::TRANSPARENT => {
            Some(egui::Rect::from_points(&path.points))
        }
        _ => None,
    }
}

/// Every stroke of a frame that is small enough to be a glyph and lands in
/// the message list — the column's own width, below the floating title bar,
/// which is where its rows are. In the list that is the paperclip and
/// nothing else: a row's edges span the column, and the title bar's own
/// glyphs are above it.
pub(super) fn list_glyph_strokes(
    shapes: &[egui::epaint::ClippedShape],
    column: egui::Rect,
) -> Vec<egui::Rect> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_glyph_strokes(&clipped.shape, column, &mut out);
    }
    out
}

pub(super) fn collect_glyph_strokes(
    shape: &egui::Shape,
    column: egui::Rect,
    out: &mut Vec<egui::Rect>,
) {
    if let egui::Shape::Vec(shapes) = shape {
        for shape in shapes {
            collect_glyph_strokes(shape, column, out);
        }
        return;
    }
    let Some(bounds) = stroke_bounds(shape) else {
        return;
    };
    let centre = bounds.center();
    if bounds.width() <= ROW_CLIP_SIZE + 2.0
        && bounds.height() <= ROW_CLIP_SIZE + 2.0
        && centre.x > column.left()
        && centre.x < column.right()
        && centre.y > TITLEBAR_HEIGHT
    {
        out.push(bounds);
    }
}

/// The centres of the stroked circles a frame drew in `area` — the
/// magnifier both search bars draw (`paint_search_icon`) is one.
pub(super) fn stroked_circles_in(
    shapes: &[egui::epaint::ClippedShape],
    area: egui::Rect,
) -> Vec<egui::Pos2> {
    fn walk(shape: &egui::Shape, area: egui::Rect, out: &mut Vec<egui::Pos2>) {
        match shape {
            egui::Shape::Circle(circle) if circle.stroke.width > 0.0 => {
                if area.contains(circle.center) {
                    out.push(circle.center);
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    walk(shape, area, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for clipped in shapes {
        walk(&clipped.shape, area, &mut out);
    }
    out
}

/// Where every straight stroke the frame drew is centred. A cross drawn by
/// `paint_close_icon` is two strokes whose midpoints are the same point, so
/// a button of that shape — and only that shape — is found by asking for it
/// rather than by working out where it ought to be.
pub(super) fn stroke_centres(
    shapes: &[egui::epaint::ClippedShape],
    area: egui::Rect,
) -> Vec<egui::Pos2> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_stroke_centres(&clipped.shape, area, &mut out);
    }
    out
}

pub(super) fn collect_stroke_centres(
    shape: &egui::Shape,
    area: egui::Rect,
    out: &mut Vec<egui::Pos2>,
) {
    match shape {
        egui::Shape::LineSegment { points, .. } => {
            let centre = egui::pos2(
                (points[0].x + points[1].x) / 2.0,
                (points[0].y + points[1].y) / 2.0,
            );
            if area.contains(centre) {
                out.push(centre);
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_stroke_centres(shape, area, out);
            }
        }
        _ => {}
    }
}

/// The rects of the attachment cards a frame drew: a card's chrome is a
/// rounded rect exactly one card high with a border on it.
pub(super) fn attachment_card_rects(
    shapes: &[egui::epaint::ClippedShape],
    area: egui::Rect,
) -> Vec<egui::Rect> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_card_rects(&clipped.shape, area, &mut out);
    }
    out
}

pub(super) fn collect_card_rects(shape: &egui::Shape, area: egui::Rect, out: &mut Vec<egui::Rect>) {
    match shape {
        egui::Shape::Rect(rect) => {
            if rect.stroke.width > 0.0
                && (rect.rect.height() - ATTACHMENT_CARD_HEIGHT).abs() < 0.5
                && area.intersects(rect.rect)
            {
                out.push(rect.rect);
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_card_rects(shape, area, out);
            }
        }
        _ => {}
    }
}

/// A dropped file, as the window hands it to egui: what a test drops on the
/// compose card. The backend owns the real one (`egui::DroppedFile`), and all
/// this asks of it is where the file is and what is in it.
#[derive(Debug)]
pub(super) struct DroppedFile(std::path::PathBuf);

impl egui::DroppedFile for DroppedFile {
    fn path(&self) -> &std::path::Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|error| error.to_string())
    }
}

/// Drop files on the window: one frame with them in the input, the way a
/// file manager hands them over.
pub(super) fn drop_files(ctx: &egui::Context, app: &mut KimoApp, paths: Vec<std::path::PathBuf>) {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1400.0, 900.0),
        )),
        dropped_files: paths
            .into_iter()
            .map(|path| std::sync::Arc::new(DroppedFile(path)) as egui::DroppedFileHandle)
            .collect(),
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| app.ui(ui));
    output.textures_delta.clear();
}

/// Every mail shown, in the folder's own order: what the list is derived from
/// with no filter and no sort of its own, for the tests that drive
/// `list_rows_of` themselves — the filter's and the sort's own tests go
/// through the app, which builds the mask and the order from
/// `ListFilter::matches` and `ListSort::compare`.
pub(super) fn all_in_order(mails: &[MailMessage]) -> Vec<usize> {
    (0..mails.len()).collect()
}

/// Every rectangle one frame filled in a given color, however deeply egui
/// nested it, for the tests that count surfaces rather than click them. A
/// bar or a panel draws its fill the same way, so what the caller asks for
/// is told apart by where it is, not by its color alone.
pub(super) fn fills_in_color(
    shapes: &[egui::epaint::ClippedShape],
    fill: egui::Color32,
) -> Vec<egui::Rect> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_fills_in_color(&clipped.shape, fill, &mut out);
    }
    out
}

/// The centres of every filled circle a frame drew in this colour: what an
/// avatar is, and the one shape on a card nothing else draws.
pub(super) fn circles_in_color(
    shapes: &[egui::epaint::ClippedShape],
    fill: egui::Color32,
) -> Vec<egui::Pos2> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_circles(&clipped.shape, fill, &mut out);
    }
    out
}

pub(super) fn collect_circles(shape: &egui::Shape, fill: egui::Color32, out: &mut Vec<egui::Pos2>) {
    match shape {
        egui::Shape::Circle(circle) if circle.fill == fill => out.push(circle.center),
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_circles(shape, fill, out);
            }
        }
        _ => {}
    }
}

pub(super) fn collect_fills_in_color(
    shape: &egui::Shape,
    fill: egui::Color32,
    out: &mut Vec<egui::Rect>,
) {
    match shape {
        egui::Shape::Rect(rect) if rect.fill == fill => out.push(rect.rect),
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_fills_in_color(shape, fill, out);
            }
        }
        _ => {}
    }
}

/// The card the message list drew: the content surface inside the list
/// column. The pane's card is the same surface on the other side of the
/// splitter, which is what the column test tells the two apart by.
pub(super) fn list_card_in(
    shapes: &[egui::epaint::ClippedShape],
    column: egui::Rect,
    content: egui::Color32,
) -> Option<egui::Rect> {
    fills_in_color(shapes, content)
        .into_iter()
        .find(|rect| column.contains(rect.center()))
}
/// Every filled rectangle of a frame that is exactly this size, however
/// deeply egui nested it. A button's hover fill is what the cell test below
/// looks for: a `TITLEBAR_BUTTON_SIZE` square.
pub(super) fn fills_of_size(
    shapes: &[egui::epaint::ClippedShape],
    size: egui::Vec2,
) -> Vec<egui::epaint::RectShape> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_fills(&clipped.shape, size, &mut out);
    }
    out
}

pub(super) fn collect_fills(
    shape: &egui::Shape,
    size: egui::Vec2,
    out: &mut Vec<egui::epaint::RectShape>,
) {
    match shape {
        egui::Shape::Rect(rect) if rect.rect.size() == size => out.push(rect.clone()),
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_fills(shape, size, out);
            }
        }
        _ => {}
    }
}

/// What the pointer fills when it is put at `center`: the `size`-square cell
/// under it, if the surface there drew one — the bar's cells at
/// `TITLEBAR_BUTTON_SIZE`, a mail card's at `CARD_ACTION_SIZE`. The pointer is
/// moved in one frame and the shapes are read from the next — egui
/// hit-tests against the last frame's rects, so a single frame would still
/// be filling the cell the pointer left.
pub(super) fn fill_under_the_pointer(
    ctx: &egui::Context,
    app: &mut KimoApp,
    width: f32,
    center: egui::Pos2,
    size: f32,
) -> Option<egui::epaint::RectShape> {
    frame_shapes_with(ctx, app, width, vec![egui::Event::PointerMoved(center)]);
    let shapes = frame_shapes_with(ctx, app, width, Vec::new());
    fills_of_size(&shapes, egui::Vec2::splat(size))
        .into_iter()
        .find(|fill| fill.rect.center() == center)
}

/// Give one of `app_with_messages`' mails an HTML body, so the pane has a
/// page for Blitz to draw.
pub(super) fn html_body(app: &mut KimoApp, index: usize) {
    app.states
        .get_mut("acct")
        .unwrap()
        .mailbox
        .folders
        .first_mut()
        .unwrap()
        .messages[index]
        .body_html = Some("<p>The mail.</p>".to_string());
}

/// A finished Blitz page for a test: one tile, and the page box the test
/// knows. Answers the tile's texture too, so a frame can be asked where the
/// page was drawn relative to the paper under it. `fill` is what the mail
/// "painted" — an opaque grey to stand in for a page, `TRANSPARENT` for a
/// mail that painted nothing, which is the case the paper exists for.
pub(super) fn ready_blitz_page(
    ctx: &egui::Context,
    size: egui::Vec2,
    fill: egui::Color32,
) -> (BlitzPage, egui::TextureId) {
    let [r, g, b, a] = fill.to_array();
    let pixels = [r, g, b, a].repeat(4);
    let image = egui::ColorImage::from_rgba_premultiplied([2, 2], &pixels);
    let texture = ctx.load_texture("test-blitz-tile", image, egui::TextureOptions::LINEAR);
    let tile = texture.id();
    let page = BlitzPage::Ready(RasterizedPage {
        tiles: vec![RasterTile {
            texture,
            offset: egui::Vec2::ZERO,
            size,
        }],
        size,
        links: Vec::new(),
        truncated: false,
    });
    (page, tile)
}

/// Show one mail in Blitz mode with a page already rendered for it, and answer
/// the frame that drew it. The pane is emptied first so the first frame
/// records the key this geometry wants (`pending_blitz_key`); the page is
/// then put under exactly that key, and the second frame draws it instead of
/// starting a render of its own — which is what lets a test look at a Blitz
/// pane without running the engine.
pub(super) fn blitz_frame_with_page(
    ctx: &egui::Context,
    app: &mut KimoApp,
    uid: &str,
    size: egui::Vec2,
) -> (Vec<egui::epaint::ClippedShape>, egui::TextureId) {
    app.html_renderer = HtmlRenderer::Blitz;
    app.selected_message = Some(uid.to_string());
    app.blitz_pages.clear();
    app.blitz_order.clear();
    app.pending_blitz_key = None;
    frame_shapes_at(ctx, app, 1400.0);
    let (key, _) = app
        .pending_blitz_key
        .clone()
        .expect("the pane asks for a render of this mail");
    let (page, tile) = ready_blitz_page(ctx, size, egui::Color32::from_gray(128));
    app.blitz_pages.insert(key, page);
    (frame_shapes_at(ctx, app, 1400.0), tile)
}

/// True when a frame filled a rectangle of exactly the page's size in this
/// colour: the paper the Blitz pane drew under the mail.
pub(super) fn paper_fill(
    shapes: &[egui::epaint::ClippedShape],
    size: egui::Vec2,
    color: egui::Color32,
) -> bool {
    fills_in_color(shapes, color)
        .into_iter()
        .any(|rect| (rect.size() - size).length() < 0.5)
}

/// Every shape a frame drew, depth first, in the order it drew them: what the
/// paper test needs in order to ask whether the paper went under the page.
pub(super) fn shapes_in_order(shapes: &[egui::epaint::ClippedShape]) -> Vec<egui::Shape> {
    fn walk(shape: &egui::Shape, out: &mut Vec<egui::Shape>) {
        match shape {
            egui::Shape::Vec(inner) => {
                for shape in inner {
                    walk(shape, out);
                }
            }
            other => out.push(other.clone()),
        }
    }
    let mut out = Vec::new();
    for clipped in shapes {
        walk(&clipped.shape, &mut out);
    }
    out
}

pub(super) fn test_account(host: &str) -> Account {
    Account {
        id: "work".to_string(),
        name: "Work".to_string(),
        protocol: Protocol::Imap,
        host: host.to_string(),
        port: 993,
        username: "me@example.com".to_string(),
        password: "hunter2".to_string(),
        tls: TlsMode::Implicit,
        smtp: None,
        provider: None,
        provider_config: None,
        access_token: None,
        refresh_token: None,
        token_expires_at: None,
    }
}

/// One mail to answer, with everything the three composes read out of it:
/// a sender, two recipients (one of them the user), a dated subject and
/// a two-line body.
pub(super) fn mail_to_answer() -> MailMessage {
    MailMessage {
        uid: "0".to_string(),
        subject: "The report".to_string(),
        from: "Ada <ada@example.com>".to_string(),
        to: "me@example.com, Bob <bob@example.com>".to_string(),
        date: "Tue, 2026-01-01".to_string(),
        timestamp: Some(1_700_000_000),
        seen: false,
        flagged: false,
        message_id: "m0".to_string(),
        in_reply_to: Vec::new(),
        references: Vec::new(),
        body_text: "Here it is.\nSecond line.".to_string(),
        body_html: None,
        preview: "Here it is.".to_string(),
        attachments: Vec::new(),
        inline_images: Vec::new(),
        labels: BTreeSet::new(),
    }
}

/// Where a mail's action cells sit, on the pane's first card: the right
/// end of its head row — the sender's own row, above the subject —
/// against the card's own padding, delete outermost as the title bar has
/// it. `from_right` counts cells from the right: 0 delete, 1 label,
/// 2 forward, 3 reply all, 4 reply — the bar's own run with flag and
/// export left out.
pub(super) fn header_action_center(from_right: usize) -> egui::Pos2 {
    let stride = CARD_ACTION_SIZE + TITLEBAR_BUTTON_GAP;
    let row_height = AVATAR_DIAMETER.max(CARD_ACTION_SIZE);
    egui::pos2(
        1400.0
            - CONTENT_GUTTER
            - CONTENT_PADDING.x
            - CARD_ACTION_SIZE / 2.0
            - stride * from_right as f32,
        TITLEBAR_HEIGHT + CONTENT_GUTTER + CONTENT_PADDING.y + row_height / 2.0,
    )
}

/// The colors a frame drew the line-drawing glyphs inside `rect` in: the
/// paths the app's icons are, with text and fills left out.
pub(super) fn glyph_colors_in(
    shapes: &[egui::epaint::ClippedShape],
    rect: egui::Rect,
) -> Vec<egui::Color32> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_glyph_colors(&clipped.shape, rect, &mut out);
    }
    out
}

pub(super) fn collect_glyph_colors(
    shape: &egui::Shape,
    rect: egui::Rect,
    out: &mut Vec<egui::Color32>,
) {
    match shape {
        egui::Shape::Path(path) if path.fill == egui::Color32::TRANSPARENT => {
            if rect.contains_rect(egui::Rect::from_points(&path.points)) {
                if let egui::epaint::ColorMode::Solid(color) = path.stroke.color {
                    out.push(color);
                }
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_glyph_colors(shape, rect, out);
            }
        }
        _ => {}
    }
}

/// The reading pane's cards, top to bottom: the content-colored fills that
/// span the pane, gutter to gutter — the mails' cards and, while a compose
/// is open on the spot, the answer's own under its mail.
pub(super) fn pane_cards(ctx: &egui::Context, app: &mut KimoApp) -> Vec<egui::Rect> {
    let pane_left = app.list_column.right();
    let mut cards: Vec<egui::Rect> =
        fills_in_color(&frame_shapes_at(ctx, app, 1400.0), theme::of(ctx).content)
            .into_iter()
            .filter(|rect| {
                rect.center().x > pane_left
                    && rect.left() <= pane_left + CONTENT_GUTTER + 0.5
                    && rect.right() >= 1400.0 - CONTENT_GUTTER - 0.5
            })
            .collect();
    cards.sort_by(|a, b| a.top().total_cmp(&b.top()));
    cards
}

/// The reply's To row's own chevron: the last of the answer card's line
/// drawings — the head bar's From list ends with one too, and it is drawn
/// first. What a chevron is, and what else a card draws, is
/// [`chevrons_in`]'s.
pub(super) fn fold_chevron(
    shapes: &[egui::epaint::ClippedShape],
    card: egui::Rect,
) -> Vec<egui::Pos2> {
    chevrons_in(shapes, card).pop().unwrap_or_default()
}

/// The chevrons a frame drew inside `area`, in the order it drew them: the
/// answer's own two line drawings — the From list's at the end of its head
/// bar, the To row's under it — found by their shape rather than by working
/// out where those rows end. What else a card draws is its head's rule (a
/// plain segment), Send (two fills) and Discard (a two-point line and two
/// four-point ones).
pub(super) fn chevrons_in(
    shapes: &[egui::epaint::ClippedShape],
    area: egui::Rect,
) -> Vec<Vec<egui::Pos2>> {
    three_point_paths(shapes, area, true)
}

/// The filled ones: the solid triangle egui gives a combo box is a
/// three-point path with a fill, where a chevron is a line drawing and has
/// none at all.
pub(super) fn triangles_in(
    shapes: &[egui::epaint::ClippedShape],
    area: egui::Rect,
) -> Vec<Vec<egui::Pos2>> {
    three_point_paths(shapes, area, false)
}

/// Frames enough for a fade to be over: a modal comes in over a few of them,
/// and a shape drawn while it is still coming in is a shape at half its
/// colour — which is what a test that reads colours has to wait out.
pub(super) const FADE_FRAMES: usize = 16;

/// The boxes a frame drew a line around, inside `area`, with the colour each
/// line was drawn in: the rounded rects with a stroke on them, which is what
/// a box drawn by hand is — a field's (`Field`), the writing area's
/// (`compose_body`), the outline buttons' own. A fill is a rect without one,
/// and a glyph is a path or a segment (`stroke_bounds`).
pub(super) fn stroked_boxes_in(
    shapes: &[egui::epaint::ClippedShape],
    area: egui::Rect,
) -> Vec<(egui::Rect, egui::Color32)> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_stroked_boxes(&clipped.shape, area, &mut out);
    }
    out
}

pub(super) fn collect_stroked_boxes(
    shape: &egui::Shape,
    area: egui::Rect,
    out: &mut Vec<(egui::Rect, egui::Color32)>,
) {
    match shape {
        egui::Shape::Rect(rect) if rect.stroke.width > 0.0 => {
            if area.contains_rect(rect.rect) {
                out.push((rect.rect, rect.stroke.color));
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_stroked_boxes(shape, area, out);
            }
        }
        _ => {}
    }
}

/// The three-point paths a frame drew inside `area`, as the points each was
/// drawn through: with `unfilled`, the line drawings — the answer's
/// chevrons — and without it the filled ones, which a three-point path can
/// only be a triangle.
pub(super) fn three_point_paths(
    shapes: &[egui::epaint::ClippedShape],
    area: egui::Rect,
    unfilled: bool,
) -> Vec<Vec<egui::Pos2>> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_three_point_paths(&clipped.shape, area, unfilled, &mut out);
    }
    out
}

pub(super) fn collect_three_point_paths(
    shape: &egui::Shape,
    area: egui::Rect,
    unfilled: bool,
    out: &mut Vec<Vec<egui::Pos2>>,
) {
    match shape {
        egui::Shape::Path(path)
            if path.points.len() == 3 && (path.fill == egui::Color32::TRANSPARENT) == unfilled =>
        {
            let bounds = egui::Rect::from_points(&path.points);
            if area.contains_rect(bounds) {
                out.push(path.points.clone());
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_three_point_paths(shape, area, unfilled, out);
            }
        }
        _ => {}
    }
}

/// The rules a frame drew inside `area`: its horizontal segments wide
/// enough to span a card. `ui.separator` is one, and a glyph is not — the
/// trash can's lid is a two-point *path*, not a segment, and nothing else
/// on a card runs its width.
pub(super) fn horizontal_rules(
    shapes: &[egui::epaint::ClippedShape],
    area: egui::Rect,
) -> Vec<egui::Rect> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_horizontal_rules(&clipped.shape, area, &mut out);
    }
    out
}

pub(super) fn collect_horizontal_rules(
    shape: &egui::Shape,
    area: egui::Rect,
    out: &mut Vec<egui::Rect>,
) {
    match shape {
        egui::Shape::LineSegment { points, .. } => {
            let bounds = egui::Rect::from_two_pos(points[0], points[1]);
            if bounds.width() > area.width() * 0.5
                && bounds.height() < 1.0
                && area.contains_rect(bounds)
            {
                out.push(bounds);
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_horizontal_rules(shape, area, out);
            }
        }
        _ => {}
    }
}

/// The boxes of every run of text a frame drew as exactly `text`, inside
/// `rect`, in the order they were painted. The same name can be drawn
/// twice — the navigation column's label group, and a popup over it — and
/// the one painted last is the one on top.
pub(super) fn text_boxes_of(
    shapes: &[egui::epaint::ClippedShape],
    rect: egui::Rect,
    text: &str,
) -> Vec<egui::Rect> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_text_boxes_of(&clipped.shape, rect, text, &mut out);
    }
    out
}

/// The colour a frame drew a run of exactly `text` in, inside `area` — the
/// glyph's own ink, which is what a button's word carries.
pub(super) fn text_color_in(
    shapes: &[egui::epaint::ClippedShape],
    area: egui::Rect,
    text: &str,
) -> Option<egui::Color32> {
    fn walk(shape: &egui::Shape, area: egui::Rect, text: &str, out: &mut Option<egui::Color32>) {
        match shape {
            egui::Shape::Text(run) => {
                let drawn = egui::Rect::from_min_size(run.pos, run.galley.size());
                if run.galley.text() == text && area.contains_rect(drawn) {
                    // What the tessellator would use: the shape's override,
                    // then the section's own colour (a galley laid out with
                    // one carries it), and only then the fallback.
                    *out = Some(
                        run.override_text_color
                            .or_else(|| run.galley.job.sections.first().map(|s| s.format.color))
                            .filter(|color| *color != egui::Color32::PLACEHOLDER)
                            .unwrap_or(run.fallback_color),
                    );
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    walk(shape, area, text, out);
                }
            }
            _ => {}
        }
    }
    let mut out = None;
    for clipped in shapes {
        walk(&clipped.shape, area, text, &mut out);
    }
    out
}

pub(super) fn collect_text_boxes_of(
    shape: &egui::Shape,
    rect: egui::Rect,
    text: &str,
    out: &mut Vec<egui::Rect>,
) {
    match shape {
        egui::Shape::Text(run) => {
            let drawn = egui::Rect::from_min_size(run.pos, run.galley.size());
            if run.galley.text() == text && rect.contains_rect(drawn) {
                out.push(drawn);
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_text_boxes_of(shape, rect, text, out);
            }
        }
        _ => {}
    }
}

/// Every image a frame drew inside `rect`, in the order it drew them — an
/// image being a textured mesh, which is the shape epaint makes one from.
pub(super) fn image_rects_of(
    shapes: &[egui::epaint::ClippedShape],
    rect: egui::Rect,
) -> Vec<egui::Rect> {
    let mut out = Vec::new();
    for clipped in shapes {
        collect_image_rects(&clipped.shape, rect, &mut out);
    }
    out
}

pub(super) fn collect_image_rects(
    shape: &egui::Shape,
    rect: egui::Rect,
    out: &mut Vec<egui::Rect>,
) {
    match shape {
        egui::Shape::Mesh(mesh) => {
            let drawn = mesh.calc_bounds();
            if rect.contains_rect(drawn) {
                out.push(drawn);
            }
        }
        // An un-rotated `Image` is a texture-filled rectangle rather than
        // a mesh (egui 0.36), and it is still an image to a test.
        egui::Shape::Rect(drawn) if drawn.brush.is_some() => {
            if rect.contains_rect(drawn.rect) {
                out.push(drawn.rect);
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_image_rects(shape, rect, out);
            }
        }
        _ => {}
    }
}

/// The navigation column's own rect this frame: everything left of the
/// message list.
pub(super) fn nav_column(app: &KimoApp) -> egui::Rect {
    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(app.list_column.left(), 900.0))
}

/// The part of the window a label popup opens over: right of the navigation
/// column and below the title bar, which is where both cells that open one
/// sit — and what keeps the column's own label row out of a search for a
/// name the popup draws too.
pub(super) fn popup_area() -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(400.0, TITLEBAR_HEIGHT),
        egui::pos2(1400.0, 900.0),
    )
}

/// An app with two accounts, each holding one INBOX of `mails` mails named
/// `<account>-<n>`, one second apart. What the label tests need: a label
/// spans the whole mailbox — every account and folder at once — and the
/// navigation column's label group stands under all of them.
pub(super) fn app_with_two_accounts(ctx: &egui::Context, mails: usize) -> KimoApp {
    let mut app = app_with_messages(ctx, 0);
    app.accounts = ["acct", "other"]
        .iter()
        .map(|id| Account {
            id: id.to_string(),
            name: id.to_string(),
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
        })
        .collect();
    let mut states = HashMap::new();
    for account in &app.accounts {
        let messages: Vec<MailMessage> = (0..mails)
            .map(|index| {
                let mut mail = test_message(&format!("{}-{index}", account.id), false);
                mail.timestamp = Some(1_700_000_000 + index as i64);
                mail
            })
            .collect();
        states.insert(
            account.id.clone(),
            AccountUiState {
                folders: vec!["INBOX".to_string()],
                mailbox: storage::Mailbox {
                    folders: vec![storage::Folder {
                        name: "INBOX".to_string(),
                        uidvalidity: Some(1),
                        synced: true,
                        listed: true,
                        can_load_older: false,
                        messages,
                    }],
                },
                ..Default::default()
            },
        );
    }
    app.states = states;
    app.selected_account = Some("acct".to_string());
    app.selected_folder = Some("INBOX".to_string());
    app
}

/// Put one label on a mail of the fixture, the way the app's own copy of the
/// mail's labels has to be for anything to be derived from it: the marks are
/// the message's own (`MailMessage::labels`), and what the view reads.
pub(super) fn label_mail(app: &mut KimoApp, account: &str, folder: &str, uid: &str, name: &str) {
    {
        let mail = app
            .states
            .get_mut(account)
            .and_then(|state| {
                state
                    .mailbox
                    .folders
                    .iter_mut()
                    .find(|candidate| candidate.name == folder)
            })
            .and_then(|folder| folder.messages.iter_mut().find(|mail| mail.uid == uid))
            .unwrap_or_else(|| panic!("no mail {uid} in {account}/{folder}"));
        mail.labels.insert(name.to_string());
    }
    if !app.labels.iter().any(|label| label == name) {
        app.labels.push(name.to_string());
        app.labels.sort_by_key(|name| name.to_lowercase());
    }
    app.labels_rev = app.labels_rev.wrapping_add(1);
}

/// Click one label's row in the navigation column: the whole row is the
/// target, and its name is what it draws.
pub(super) fn click_label(ctx: &egui::Context, app: &mut KimoApp, name: &str) {
    let shapes = frame_shapes_at(ctx, app, 1400.0);
    let row = text_boxes_of(&shapes, nav_column(app), name)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("the column draws no row for {name}"));
    click_at(ctx, app, row.center());
}

/// Where the bar's filter cell sits: the search box's own group, a cell's gap
/// to its right (`titlebar_anchors`). Runs a frame, so the columns the plan
/// is measured against are the ones the bar itself read.
pub(super) fn titlebar_filter_rect(
    ctx: &egui::Context,
    app: &mut KimoApp,
    width: f32,
) -> egui::Rect {
    frame_shapes_at(ctx, app, width);
    let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, TITLEBAR_HEIGHT));
    let rects = titlebar_rects(bar, 3);
    let pane_left = if app.pane_popup {
        0.0
    } else {
        app.list_column.right()
    };
    titlebar_anchors(
        bar,
        first_group_end(),
        app.list_column.left(),
        pane_left,
        rects.sync.left(),
    )
    .filter
    .expect("the bar draws a filter cell here")
}

/// Where the bar's sort cell sits: the filter's other half, a cell's gap to
/// its right in the same group (`titlebar_anchors`). Runs a frame, like
/// `titlebar_filter_rect`.
pub(super) fn titlebar_sort_rect(ctx: &egui::Context, app: &mut KimoApp, width: f32) -> egui::Rect {
    frame_shapes_at(ctx, app, width);
    let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, TITLEBAR_HEIGHT));
    let rects = titlebar_rects(bar, 3);
    let pane_left = if app.pane_popup {
        0.0
    } else {
        app.list_column.right()
    };
    titlebar_anchors(
        bar,
        first_group_end(),
        app.list_column.left(),
        pane_left,
        rects.sync.left(),
    )
    .sort
    .expect("the bar draws a sort cell here")
}

/// Where one of the title bar's mail action cells sits: the bar's own
/// arithmetic, from the search box the run is laid out after onwards
/// (`titlebar_rects`, `titlebar_anchors`).
pub(super) fn titlebar_action_center(
    ctx: &egui::Context,
    app: &mut KimoApp,
    action: MailAction,
    width: f32,
) -> egui::Pos2 {
    let actions = mail_action_rect(ctx, app, width)
        .unwrap_or_else(|| panic!("the bar drew no mail actions at {width} pt"));
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
}

/// Where the first group ends: the nav toggle and the pen, a cell's gap
/// between them and the bar's own margin before the first. The one anchor
/// nothing else may cross.
pub(super) fn first_group_end() -> f32 {
    TITLEBAR_LEFT_INSET + TITLEBAR_BUTTON_GAP + 2.0 * TITLEBAR_BUTTON_SIZE
}

/// The rect the bar gives its mail actions this frame, from the columns the
/// app has just laid out — `None` when the pane is too narrow to hold
/// them beside the window controls. Runs a frame, so the app's column rects
/// are the ones the bar itself read.
pub(super) fn mail_action_rect(
    ctx: &egui::Context,
    app: &mut KimoApp,
    width: f32,
) -> Option<egui::Rect> {
    frame_shapes_at(ctx, app, width);
    let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, TITLEBAR_HEIGHT));
    let rects = titlebar_rects(bar, 3);
    let pane_left = if app.pane_popup {
        0.0
    } else {
        app.list_column.right()
    };
    titlebar_anchors(
        bar,
        first_group_end(),
        app.list_column.left(),
        pane_left,
        rects.sync.left(),
    )
    .actions
}

/// The dump canvas: the height every picture is drawn at, and the width the
/// window is asked for.
pub(super) const DUMP_HEIGHT: usize = 900;

/// The window the dump tool draws at: 1400 by 900 unless `KIMO_UI_WIDTH`
/// says otherwise, which is how the narrow layouts — the capped pane, the
/// popup one — are looked at.
pub(super) fn dump_width() -> f32 {
    std::env::var("KIMO_UI_WIDTH")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1400.0)
}

/// One headless frame of the app, rasterized straight from egui's own
/// triangles: a frame is a mesh per texture with premultiplied vertex
/// colors, so what a real backend would draw is a barycentric fill each.
/// The font atlas (and any other texture the frame uploaded) is sampled for
/// it; there is no anti-aliasing to do — egui has already feathered every
/// edge into the vertices.
pub(super) fn render_frame_png(
    ctx: &egui::Context,
    app: &mut KimoApp,
    path: &std::path::Path,
    textures: &mut HashMap<egui::TextureId, (usize, usize, Vec<egui::Color32>)>,
) {
    /// The dump canvas height.
    const DUMP_HEIGHT: usize = 900;
    const HEIGHT: usize = DUMP_HEIGHT;
    let width = dump_width() as usize;

    let shapes = collect_frame(ctx, app, textures, Vec::new());
    let ppp = ctx.pixels_per_point();
    let primitives = ctx.tessellate(shapes, ppp);
    let mut canvas = vec![egui::Color32::TRANSPARENT; width * HEIGHT];
    for primitive in &primitives {
        let egui::epaint::Primitive::Mesh(mesh) = &primitive.primitive else {
            continue;
        };
        let Some((tw, th, texels)) = textures.get(&mesh.texture_id) else {
            continue;
        };
        for triangle in mesh.indices.as_chunks::<3>().0 {
            let v = [
                mesh.vertices[triangle[0] as usize],
                mesh.vertices[triangle[1] as usize],
                mesh.vertices[triangle[2] as usize],
            ];
            let xs = v.map(|vertex| vertex.pos.x * ppp);
            let ys = v.map(|vertex| vertex.pos.y * ppp);
            let (x0, x1) = (
                xs.iter()
                    .cloned()
                    .fold(f32::INFINITY, f32::min)
                    .floor()
                    .max(0.0) as usize,
                xs.iter()
                    .cloned()
                    .fold(f32::NEG_INFINITY, f32::max)
                    .ceil()
                    .min(width as f32) as usize,
            );
            let (y0, y1) = (
                ys.iter()
                    .cloned()
                    .fold(f32::INFINITY, f32::min)
                    .floor()
                    .max(0.0) as usize,
                ys.iter()
                    .cloned()
                    .fold(f32::NEG_INFINITY, f32::max)
                    .ceil()
                    .min(HEIGHT as f32) as usize,
            );
            for y in y0..y1 {
                for x in x0..x1 {
                    let point = egui::pos2((x as f32 + 0.5) / ppp, (y as f32 + 0.5) / ppp);
                    if !primitive.clip_rect.contains(point) {
                        continue;
                    }
                    let Some(weights) = barycentric(v.map(|vertex| vertex.pos), point) else {
                        continue;
                    };
                    if weights.iter().any(|weight| *weight < 0.0) {
                        continue;
                    }
                    let uv = egui::pos2(
                        v[0].uv.x * weights[0] + v[1].uv.x * weights[1] + v[2].uv.x * weights[2],
                        v[0].uv.y * weights[0] + v[1].uv.y * weights[1] + v[2].uv.y * weights[2],
                    );
                    let tx = ((uv.x * *tw as f32) as usize).min(tw.saturating_sub(1));
                    let ty = ((uv.y * *th as f32) as usize).min(th.saturating_sub(1));
                    let texel = texels[ty * *tw + tx];
                    let source = v[0].color;
                    let color = egui::Color32::from_rgba_premultiplied(
                        (source.r() as u32 * texel.r() as u32 / 255) as u8,
                        (source.g() as u32 * texel.g() as u32 / 255) as u8,
                        (source.b() as u32 * texel.b() as u32 / 255) as u8,
                        (source.a() as u32 * texel.a() as u32 / 255) as u8,
                    );
                    let dst = canvas[y * width + x];
                    let alpha = color.a() as u32;
                    canvas[y * width + x] = egui::Color32::from_rgba_premultiplied(
                        (color.r() as u32 + dst.r() as u32 * (255 - alpha) / 255) as u8,
                        (color.g() as u32 + dst.g() as u32 * (255 - alpha) / 255) as u8,
                        (color.b() as u32 + dst.b() as u32 * (255 - alpha) / 255) as u8,
                        (alpha + dst.a() as u32 * (255 - alpha) / 255) as u8,
                    );
                }
            }
        }
    }
    let bytes: Vec<u8> = canvas.iter().flat_map(|pixel| pixel.to_array()).collect();
    let image = image::RgbaImage::from_raw(width as u32, HEIGHT as u32, bytes)
        .expect("the canvas is the size it was allocated");
    image.save(path).expect("the dump directory is writable");
}

/// Run one frame of the app and keep the textures it brought: the font
/// atlas arrives on the frame that first draws a glyph and every frame after
/// it patches what is there, so a rasterizer that misses one frame draws
/// that frame's glyphs as nothing. Every frame a dump drives — the clicks
/// included — has to come through here.
pub(super) fn collect_frame(
    ctx: &egui::Context,
    app: &mut KimoApp,
    textures: &mut HashMap<egui::TextureId, (usize, usize, Vec<egui::Color32>)>,
    events: Vec<egui::Event>,
) -> Vec<egui::epaint::ClippedShape> {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(dump_width(), DUMP_HEIGHT as f32),
        )),
        events,
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| app.ui(ui));
    apply_textures(&output.textures_delta, textures);
    output.textures_delta.clear();
    output.shapes
}

/// A click, frame by frame, through `collect_frame` — the pointer moved onto
/// the target and then the button down and up.
pub(super) fn dump_click(
    ctx: &egui::Context,
    app: &mut KimoApp,
    textures: &mut HashMap<egui::TextureId, (usize, usize, Vec<egui::Color32>)>,
    pos: egui::Pos2,
) {
    collect_frame(ctx, app, textures, vec![egui::Event::PointerMoved(pos)]);
    for pressed in [true, false] {
        collect_frame(
            ctx,
            app,
            textures,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            }],
        );
    }
}

/// Keep the textures a frame uploaded or patched: the font atlas whole the
/// first time, in patches as it grows, and any picture the mail brought.
pub(super) fn apply_textures(
    delta: &egui::TexturesDelta,
    textures: &mut HashMap<egui::TextureId, (usize, usize, Vec<egui::Color32>)>,
) {
    for (id, deltas) in &delta.set {
        for delta in deltas {
            let egui::ImageData::Color(image) = &delta.image;
            let (width, height) = (image.size[0], image.size[1]);
            let Some([x0, y0]) = delta.pos else {
                // A whole texture, and the font atlas is sent whole again
                // whenever it grows: it replaces what was there, or the
                // glyphs past the old edge would be missing.
                textures.insert(*id, (width, height, image.pixels.clone()));
                continue;
            };
            let Some(target) = textures.get_mut(id) else {
                continue;
            };
            for y in 0..height {
                for x in 0..width {
                    let (tx, ty) = (x0 + x, y0 + y);
                    if tx < target.0 && ty < target.1 {
                        target.2[ty * target.0 + tx] = image.pixels[y * width + x];
                    }
                }
            }
        }
    }
}

/// The three barycentric weights of a point in a triangle, or `None` when
/// the triangle has no area.
pub(super) fn barycentric(triangle: [egui::Pos2; 3], point: egui::Pos2) -> Option<[f32; 3]> {
    let [a, b, c] = triangle;
    let area = (b.x - a.x) * (c.y - a.y) - (c.x - a.x) * (b.y - a.y);
    if area.abs() < f32::EPSILON {
        return None;
    }
    let first = ((b.x - point.x) * (c.y - point.y) - (c.x - point.x) * (b.y - point.y)) / area;
    let second = ((c.x - point.x) * (a.y - point.y) - (a.x - point.x) * (c.y - point.y)) / area;
    Some([first, second, 1.0 - first - second])
}
