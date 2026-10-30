use super::super::*;
use super::common::*;

#[test]
fn html_renderer_round_trips_through_settings() {
    assert_eq!(HtmlRenderer::parse("blitz"), HtmlRenderer::Blitz);
    assert_eq!(HtmlRenderer::parse("egui"), HtmlRenderer::Simple);
    // Anything unknown — including the legacy render_mode field's "web" —
    // falls back to the built-in renderer.
    assert_eq!(HtmlRenderer::parse("web"), HtmlRenderer::Simple);
    assert_eq!(HtmlRenderer::parse(""), HtmlRenderer::Simple);
    assert_eq!(HtmlRenderer::Blitz.as_str(), "blitz");
    assert_eq!(HtmlRenderer::Simple.as_str(), "egui");
}

#[test]
fn edit_form_round_trips_and_keeps_the_account_id() {
    let account = test_account("imap.example.com");
    let form = AccountForm::edit(&account);
    assert_eq!(form.editing.as_deref(), Some("work"));
    assert_eq!(form.to_account(account.id.clone(), Some(&account)), account);
}

#[test]
fn a_plugin_account_carries_its_provider_and_config() {
    let form = AccountForm {
        plugin: Some("gmail".to_string()),
        name: "Work".to_string(),
        username: "me@gmail.com".to_string(),
        client_id: " cid-1 ".to_string(),
        smtp_enabled: true,
        ..AccountForm::default()
    };
    let account = form.to_account("work".to_string(), None);
    assert_eq!(account.provider.as_deref(), Some("gmail"));
    assert_eq!(
        account.provider_config.as_deref(),
        Some(r#"{"client_id":"cid-1"}"#)
    );
    assert_eq!(account.smtp, None, "the module files its own submission");
    assert!(account.is_plugin() && account.can_send());
}

#[test]
fn editing_keeps_the_tokens_a_provider_switch_spends_them() {
    let mut account = test_account("imap.example.com");
    account.provider = Some("gmail".to_string());
    account.access_token = Some("access".to_string());
    account.refresh_token = Some("refresh".to_string());

    let mut form = AccountForm::edit(&account);
    form.name = "Work mail".to_string();
    let saved = form.to_account(account.id.clone(), Some(&account));
    assert_eq!(saved.access_token.as_deref(), Some("access"));
    assert_eq!(saved.refresh_token.as_deref(), Some("refresh"));

    form.plugin = None;
    let spent = form.to_account(account.id.clone(), Some(&account));
    assert_eq!(spent.provider, None);
    assert_eq!(
        spent.access_token, None,
        "another provider's token is not a credential"
    );
}

#[test]
fn a_provider_change_is_another_mailbox() {
    let account = test_account("imap.example.com");
    let mut form = AccountForm::edit(&account);
    assert!(!form.points_elsewhere(&account));
    form.plugin = Some("gmail".to_string());
    assert!(form.points_elsewhere(&account));
}

/// Preferences → Appearance offers the accent as a row of chips — one per
/// choice, each drawn in the colour it stands for — and the one in force is
/// ringed in the app's own ink rather than ticked, because a chip *is* a
/// colour and a mark on it would be a second colour to read.
#[test]
fn the_appearance_page_shows_the_accents_with_the_one_in_force_ringed() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.show_preferences = true;
    app.prefs_tab = PrefsTab::Appearance;
    // The key a settings file names is what the app starts with; a click on
    // a chip is the same assignment, and what it writes there is the
    // app's own business rather than this test's.
    app.accent = Accent::parse("teal");
    for _ in 0..FADE_FRAMES {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    let scheme = theme::of(&ctx);
    let chosen = app.accent.colors(scheme.dark).accent;
    assert_eq!(
        scheme.accent, chosen,
        "the chosen accent is the one the app draws with"
    );

    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let chips: Vec<egui::epaint::RectShape> =
        fills_of_size(&shapes, egui::Vec2::splat(ACCENT_CHIP_SIZE))
            .into_iter()
            .filter(|chip| {
                Accent::ALL
                    .iter()
                    .any(|accent| accent.colors(scheme.dark).accent == chip.fill)
            })
            .collect();
    assert_eq!(chips.len(), Accent::ALL.len(), "one chip per choice");
    for accent in Accent::ALL {
        let color = accent.colors(scheme.dark).accent;
        assert!(
            chips.iter().any(|chip| chip.fill == color),
            "{} is offered: {:?}",
            accent.name,
            chips.iter().map(|chip| chip.fill).collect::<Vec<_>>()
        );
    }
    // The ring: one stroke, just outside the chip it belongs to — which has
    // to be the chosen one's.
    let ring = ACCENT_CHIP_SIZE + ACCENT_CHIP_RING_GAP * 2.0;
    let rings: Vec<(egui::Rect, egui::Color32)> = stroked_boxes_in(&shapes, popup_area())
        .into_iter()
        .filter(|(rect, _)| (rect.width() - ring).abs() < 0.5 && (rect.height() - ring).abs() < 0.5)
        .collect();
    assert_eq!(rings.len(), 1, "one chip is ringed: {rings:?}");
    assert_eq!(rings[0].1, scheme.text, "in the app's own ink");
    let ringed = chips
        .iter()
        .find(|chip| chip.rect.contains(rings[0].0.center()))
        .expect("the ring is round one of the chips");
    assert_eq!(ringed.fill, chosen, "and it is the one in force");
}

/// The preferences heading ends in a cross rather than a worded button —
/// the modal's own chrome — and a click on the cross closes the modal.
#[test]
fn the_preferences_cross_closes_the_modal() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.show_preferences = true;
    for _ in 0..FADE_FRAMES {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        !texts.iter().any(|text| text == "Close"),
        "the heading's worded button is gone: {texts:?}"
    );
    let close = ctx
        .read_response(egui::Id::new("preferences-close"))
        .expect("the cross is a widget of the heading row");
    click_at(&ctx, &mut app, close.rect.center());
    assert!(!app.show_preferences, "the cross closes the modal");
}

/// Preferences → Appearance names the CJK fallback beside the other two
/// fonts — "Automatic" until one is picked — and says what a pick does not
/// cover instead of quietly letting mail turn into boxes.
#[test]
fn the_appearance_page_offers_a_cjk_fallback_and_says_what_it_covers() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.show_preferences = true;
    app.prefs_tab = PrefsTab::Appearance;
    for _ in 0..FADE_FRAMES {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(
        !text_boxes_of(&shapes, popup_area(), "CJK fallback").is_empty(),
        "the row is on the page"
    );
    assert!(
        !text_boxes_of(&shapes, popup_area(), "Automatic").is_empty(),
        "unset is the automatic face, not a family name"
    );

    // A family whose file is gone: the pick resolves against the scan list,
    // and a face nothing can read covers none of the sample scripts.
    app.font_families = Some(vec![FontFamily {
        name: "Broken Sans".to_string(),
        path: std::path::PathBuf::from("/nonexistent/BrokenSans.ttf"),
        index: 0,
        monospace: false,
    }]);
    app.cjk_font = Some("Broken Sans".to_string());
    app.apply_fonts();
    for _ in 0..2 {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    let warning =
            "Broken Sans has no glyphs for Han, kana, Hangul, Bopomofo; CJK mail will show boxes there.";
    assert!(
        !text_boxes_of(&shapes, popup_area(), warning).is_empty(),
        "the page says what the pick does not cover"
    );
}

/// Preferences → Graphics names the cards this machine has and says what a
/// pick opens: the chosen card's driver manifests, and not the other
/// card's.
#[test]
fn the_graphics_page_lists_the_cards_and_the_drivers_a_pick_loads() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.show_preferences = true;
    app.prefs_tab = PrefsTab::Graphics;
    app.gpu_scan = Some(gpu_scan_fixture());
    app.gpu = Some("0000:03:00.0".to_string());
    for _ in 0..FADE_FRAMES {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts.iter().any(|text| text == "Battlemage G21 [Arc B580]"),
        "the card the pick names: {texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|text| text == "TU104 [GeForce RTX 2070 SUPER]"),
        "and the other one: {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("intel_icd.json")),
        "the drivers the pick opens: {texts:?}"
    );
    assert!(
        !texts.iter().any(|text| text.contains("nvidia_icd.json")),
        "and not the other card's: {texts:?}"
    );
}

/// The scan answers the page's spinner: the event puts the cards and the
/// drivers in the app's state and stops it asking.
#[test]
fn the_graphics_scan_arrives_in_the_app_state() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.gpu_scanning = true;
    app.apply_event(Event::Gpus(gpu_scan_fixture()));
    assert!(!app.gpu_scanning, "the page stops waiting");
    let scan = app.gpu_scan.expect("the scan is the page's input");
    assert_eq!(scan.cards.len(), 2);
    assert_eq!(scan.icds.len(), 4);
}

/// A saved GPU that cannot be honoured — one that is not in the machine, or
/// one that is not an address at all — falls back to every driver, and the
/// page says which value it ignored instead of leaving a pick that does
/// nothing.
#[test]
fn the_graphics_page_says_when_the_saved_gpu_cannot_be_used() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.show_preferences = true;
    app.prefs_tab = PrefsTab::Graphics;
    app.gpu_scan = Some(gpu_scan_fixture());

    app.gpu = Some("0000:0c:00.0".to_string());
    for _ in 0..FADE_FRAMES {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts
            .iter()
            .any(|text| text.contains("0000:0c:00.0") && text.contains("not installed")),
        "{texts:?}"
    );

    app.gpu = Some("the nvidia one".to_string());
    for _ in 0..2 {
        run_app_frame(&ctx, &mut app, Vec::new());
    }
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts.iter().any(|text| text.contains("does not recognise")),
        "{texts:?}"
    );
}

/// Preferences → About names the running build and draws the app's icon:
/// the version is Cargo.toml's, read at compile time, so a release cannot
/// ship a page that claims another one.
#[test]
fn the_about_page_shows_the_build_version_and_the_app_icon() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.show_preferences = true;
    app.prefs_tab = PrefsTab::About;
    for _ in 0..FADE_FRAMES {
        run_app_frame(&ctx, &mut app, Vec::new());
    }

    let texts = frame_texts(&ctx, &mut app);
    assert!(texts.iter().any(|text| text == "Kimo"), "{texts:?}");
    let version = format!("Version {}", env!("CARGO_PKG_VERSION"));
    assert!(texts.contains(&version), "{texts:?}");
    assert!(
        texts
            .iter()
            .any(|text| text == "A desktop email client written in pure Rust"),
        "{texts:?}"
    );

    let icon = app
        .app_icon
        .as_ref()
        .expect("the embedded icon decoded")
        .id();
    let shapes = frame_shapes_at(&ctx, &mut app, 1400.0);
    assert!(
        shapes
            .iter()
            .any(|clipped| draws_texture(&clipped.shape, icon)),
        "the page draws the icon it loaded"
    );
}

/// The provider picker is a preset: it fills the endpoints the vendor's
/// servers use, enables sending with them, and says what sign-in needs.
/// Custom is not a preset — it leaves what was typed alone.
#[test]
fn picking_a_provider_fills_in_its_endpoints() {
    let mut form = AccountForm::default();
    assert_eq!(form.vendor, VendorKey::Custom);
    form.name = "Personal".to_string();
    form.username = "me@example.com".to_string();

    form.vendor = VendorKey::Gmail;
    form.apply_preset();
    assert_eq!(form.protocol, Protocol::Imap);
    assert_eq!(form.host, "imap.gmail.com");
    assert_eq!(form.port_str, "993");
    assert_eq!(form.tls, TlsMode::Implicit);
    assert!(form.smtp_enabled, "a provider preset enables sending");
    assert_eq!(form.smtp_host, "smtp.gmail.com");
    assert_eq!(form.smtp_port_str, "465");
    assert_eq!(form.smtp_tls, TlsMode::Implicit);

    // Outlook is the provider Kimo cannot sign in to, and the form says so.
    form.vendor = VendorKey::Outlook;
    form.apply_preset();
    assert_eq!(form.host, "outlook.office365.com");
    assert_eq!(form.smtp_tls, TlsMode::StartTls);
    assert_eq!(form.smtp_port_str, "587");
    assert_eq!(form.vendor.profile().auth, vendor::AuthKind::OAuthOnly);

    // Custom keeps the server the user typed.
    form.vendor = VendorKey::Custom;
    let host = form.host.clone();
    form.apply_preset();
    assert_eq!(form.host, host);
}

/// The Accounts page's form draws the Provider row and the selected
/// provider's sign-in note — the two things the preset adds to the page.
#[test]
fn the_account_form_draws_the_provider_row_and_its_note() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.show_preferences = true;
    app.prefs_tab = PrefsTab::Accounts;
    app.account_form = AccountForm::default();
    app.show_account_form = true;
    // Two frames: the modal is an Area and lays itself out on the first.
    frame_texts(&ctx, &mut app);
    let texts = frame_texts(&ctx, &mut app);
    assert!(texts.iter().any(|text| text == "Provider"), "{texts:?}");
    assert!(
        texts.iter().any(|text| text == "Custom (IMAP/POP3)"),
        "the picker opens on Custom: {texts:?}"
    );

    app.account_form.vendor = VendorKey::Gmail;
    app.account_form.apply_preset();
    frame_texts(&ctx, &mut app);
    let texts = frame_texts(&ctx, &mut app);
    assert!(texts.iter().any(|text| text == "Gmail"), "{texts:?}");
    assert!(
        texts.iter().any(|text| text.contains("app password")),
        "Gmail's note says what sign-in needs: {texts:?}"
    );

    // Outlook is the one Kimo cannot sign in to, and the form says so.
    app.account_form.vendor = VendorKey::Outlook;
    app.account_form.apply_preset();
    frame_texts(&ctx, &mut app);
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts.iter().any(|text| text.contains("OAuth2")),
        "Outlook's note is drawn: {texts:?}"
    );
}

/// The form opens on the vendor its account's endpoints name, so editing a
/// Gmail account shows Gmail's note without the account storing a vendor.
#[test]
fn the_form_infers_the_vendor_from_the_accounts_endpoints() {
    let account = test_account("imap.gmail.com");
    assert_eq!(AccountForm::edit(&account).vendor, VendorKey::Gmail);
    let account = test_account("imap.example.com");
    assert_eq!(AccountForm::edit(&account).vendor, VendorKey::Custom);
}

/// A provider preset switched off in Preferences → Accounts is no longer
/// offered: the form edits its accounts as plain servers, and the vendor
/// policy the window hands the send job is Custom. The standard path can
/// never be switched off, and the other presets stay on.
#[test]
fn a_switched_off_provider_is_no_longer_a_preset() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    let mut account = app.accounts[0].clone();
    account.host = "imap.gmail.com".to_string();
    app.accounts[0] = account.clone();

    assert_eq!(app.effective_vendor(&account), VendorKey::Gmail);
    assert!(app.vendor_enabled(VendorKey::Gmail));
    assert!(app.enabled_vendors().contains(&VendorKey::Gmail));

    app.disabled_vendors.insert(VendorKey::Gmail);
    assert!(!app.vendor_enabled(VendorKey::Gmail));
    assert_eq!(app.effective_vendor(&account), VendorKey::Custom);
    assert!(!app.enabled_vendors().contains(&VendorKey::Gmail));
    // The rest of the presets and the standard path are untouched.
    assert!(app.enabled_vendors().contains(&VendorKey::Custom));
    assert!(app.enabled_vendors().contains(&VendorKey::Outlook));

    // Editing the account opens the form on Custom, not on a vendor the
    // picker no longer lists.
    app.open_edit_account_form(&account.id);
    assert_eq!(app.account_form.vendor, VendorKey::Custom);

    // And a form that was already set to the vendor falls back too: the
    // note of a switched-off provider is not drawn.
    app.show_preferences = true;
    app.prefs_tab = PrefsTab::Accounts;
    app.show_account_form = true;
    app.account_form = AccountForm::default();
    app.account_form.vendor = VendorKey::Gmail;
    frame_texts(&ctx, &mut app);
    let texts = frame_texts(&ctx, &mut app);
    assert_eq!(app.account_form.vendor, VendorKey::Custom);
    assert!(
        !texts.iter().any(|text| text.contains("app password")),
        "the disabled provider's note is not drawn: {texts:?}"
    );
}

/// The Accounts page lists every vendor preset with its own switch — a
/// switched-off one stays in the list, or there would be no way to switch
/// it back on — and the standard path is described rather than toggled.
#[test]
fn the_accounts_page_offers_a_switch_per_vendor() {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx, None, None, None);
    let mut app = app_with_messages(&ctx, 1);
    app.show_preferences = true;
    app.prefs_tab = PrefsTab::Accounts;
    app.disabled_vendors.insert(VendorKey::Gmail);
    frame_texts(&ctx, &mut app);
    let texts = frame_texts(&ctx, &mut app);
    assert!(
        texts.iter().any(|text| text == "Provider presets"),
        "the section is drawn: {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text == "Gmail"),
        "a switched-off vendor stays listed so it can come back: {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text == "Outlook / Microsoft 365"),
        "every vendor is listed: {texts:?}"
    );
    assert!(
        !texts.iter().any(|text| text == "Custom (IMAP/POP3)"),
        "Custom is not a vendor to switch: {texts:?}"
    );
}

/// The account form's Sending group: off is an account that only reads
/// mail, on needs a server and a port, and what it builds is what the
/// account gets — credentials it does not name included.
#[test]
fn the_account_form_builds_the_outgoing_server_it_describes() {
    let mut form = AccountForm::default();
    assert!(!form.is_valid(), "a new form has nothing in it");
    form.name = "Work".to_string();
    form.host = "imap.example.com".to_string();
    form.username = "me@example.com".to_string();
    assert!(form.is_valid(), "reading mail needs no outgoing server");
    assert_eq!(form.to_account("work".to_string(), None).smtp, None);

    // Switched on with nothing to reach, and then with something.
    form.smtp_enabled = true;
    assert!(!form.is_valid());
    form.smtp_host = "smtp.example.com".to_string();
    assert!(form.is_valid());
    assert_eq!(
        form.to_account("work".to_string(), None).smtp,
        Some(SmtpSettings {
            host: "smtp.example.com".to_string(),
            port: 465,
            tls: TlsMode::Implicit,
            login: None,
        })
    );

    // A login of its own is the pair below the checkbox, and the security
    // choice is what moves the port.
    form.smtp_tls = TlsMode::StartTls;
    form.smtp_port_str = SmtpSettings::default_port(form.smtp_tls).to_string();
    form.smtp_own_login = false;
    form.smtp_username = "code".to_string();
    form.smtp_password = "abcd efgh".to_string();
    let account = form.to_account("work".to_string(), None);
    assert_eq!(
        account.smtp.as_ref().map(|smtp| smtp.port),
        Some(587),
        "STARTTLS is the submission port"
    );
    assert_eq!(account.smtp_login(), Some(("code", "abcd efgh")));

    // Editing an account fills the group in from what it already has.
    let edit = AccountForm::edit(&account);
    assert!(edit.smtp_enabled);
    assert_eq!(edit.smtp_host, "smtp.example.com");
    assert!(!edit.smtp_own_login);
    assert_eq!(edit.smtp_username, "code");
    assert_eq!(
        edit.smtp(),
        account.smtp,
        "the form describes what the account has"
    );

    // An outgoing server is not part of what makes the cached mail describe
    // another mailbox: moving it must not purge what is stored.
    let mut moved = edit.clone();
    moved.smtp_host = "smtp.other.example".to_string();
    assert!(!moved.points_elsewhere(&account));
    // The incoming endpoint still is.
    let mut elsewhere = edit.clone();
    elsewhere.host = "imap.other.example".to_string();
    assert!(elsewhere.points_elsewhere(&account));
}
