//! Theme selection and the color scheme it resolves to.
//!
//! Preferences → Appearance picks a [`Theme`]; [`Theme::scheme`] resolves it
//! (including "System") to one of the two [`ColorScheme`] defaults. A scheme is
//! the single source of truth for color: [`ColorScheme::visuals`] maps it onto
//! egui's `Visuals` for the stock widgets, while the app's own painting — title
//! bar, folder rows, message cards, avatars — reads the fields directly through
//! [`of`], which hands out the scheme [`install`]ed for the current frame.
//!
//! Both schemes are consts with every slot filled in. Nothing is configurable
//! yet, so `settings.json` only stores the theme name; the purples are the
//! brand color of `assets/app.svg`.

use egui::Color32;

/// UI color scheme. "System" follows the OS preference (best-effort
/// detection, see `app::system_prefers_dark`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Theme {
    System,
    Light,
    Dark,
}

impl Theme {
    /// The choices, in the order the Appearance radios show them.
    pub const ALL: [Theme; 3] = [Theme::System, Theme::Light, Theme::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Theme::System => "System",
            Theme::Light => "Light",
            Theme::Dark => "Dark",
        }
    }

    /// Value stored in `settings.json`.
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "light" => Theme::Light,
            "dark" => Theme::Dark,
            _ => Theme::System,
        }
    }

    /// Resolve the choice to a palette; `system_dark` is the cached OS
    /// preference and only matters for [`Theme::System`].
    pub fn scheme(self, system_dark: bool) -> ColorScheme {
        match self {
            Theme::Light => LIGHT,
            Theme::Dark => DARK,
            Theme::System if system_dark => DARK,
            Theme::System => LIGHT,
        }
    }
}

/// The avatar colors, shared by both themes so that a sender keeps its color
/// when the theme changes. The initial on top is [`ColorScheme::avatar_text`].
const AVATAR: [Color32; 8] = [
    Color32::from_rgb(229, 57, 53),
    Color32::from_rgb(66, 133, 244),
    Color32::from_rgb(52, 168, 83),
    Color32::from_rgb(251, 140, 0),
    Color32::from_rgb(142, 36, 170),
    Color32::from_rgb(0, 150, 136),
    Color32::from_rgb(121, 85, 72),
    Color32::from_rgb(236, 64, 122),
];

/// Every color the application draws with. The hex values in the comments are
/// the default for each theme, spelled out in [`ColorScheme::light`] and
/// [`ColorScheme::dark`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ColorScheme {
    /// Shown in Preferences → Appearance.
    pub name: &'static str,
    /// Whether this is the dark palette. Drives egui's own light/dark widget
    /// tuning (shadows, hover expansion, popup dimming).
    pub dark: bool,

    // --- surfaces ---
    /// Chrome: the title bar and the navigation column.
    pub panel: Color32,
    /// The working surface: the message list column, and the card the reading
    /// pane floats on inside the chrome's gutter. The *lighter* of the two
    /// surfaces in both schemes — near-white paper on gray chrome in the light
    /// one, a surface lifted off the navigation in the dark one — because the
    /// card, its gutter and the list's hairlines are all the step between the
    /// two slots and nothing else.
    pub content: Color32,
    /// Background of modal windows and popups.
    pub window: Color32,
    /// Faint fills: striped rows, inset cards.
    pub card: Color32,
    /// Fill of a hovered row or widget.
    pub hover: Color32,
    /// Background of text edits and combo boxes.
    pub input: Color32,
    /// Background of inline code in HTML mail.
    pub code: Color32,
    /// Hairlines: widget outlines, window strokes, egui's own separators.
    pub border: Color32,
    /// The mail rows' edges — the left and right side of a row, and its bottom
    /// — one step *fainter* than [`ColorScheme::border`], which is what the
    /// rule between two rows used to be. Fainter, not merely lighter: the dark
    /// scheme draws its lines lighter than the surface they sit on, so there
    /// "fainter" means closer to `content`.
    pub row_edge: Color32,
    /// A row's top edge, fainter still: with the row above's bottom edge it
    /// makes the two pixels that separate two mails.
    pub row_edge_top: Color32,

    // --- text ---
    /// Body text.
    pub text: Color32,
    /// Muted text: dates, previews, read mail, hints.
    pub text_weak: Color32,
    /// Glyph drawn on [`ColorScheme::danger_hover`].
    pub on_danger: Color32,

    // --- accents ---
    /// Interactive highlight: focus rings and the busy spinner.
    pub accent: Color32,
    /// The mark drawn *on* [`ColorScheme::accent`]: the check in a checked
    /// box. Like the avatar palette it is white in both schemes.
    pub on_accent: Color32,
    /// Fill of selected rows, selected text and the active preferences tab.
    pub selection: Color32,
    /// Hyperlinks in HTML mail.
    pub link: Color32,

    // --- status ---
    /// Caution, e.g. a folder that failed to sync.
    pub warn: Color32,
    /// Errors and destructive actions.
    pub danger: Color32,
    /// Hovered window-close button.
    pub danger_hover: Color32,

    // --- message list ---
    /// Fill of a flagged mail's row, and of the same mail's card in the reading
    /// pane: the warning tint a flag is read by. Warm rather than gray, so it is
    /// never mistaken for the row's own `selection` or `hover`, and dark
    /// enough under the light scheme's text — the contrast test holds it to the
    /// same 4.5:1 as the other fills.
    pub flag: Color32,
    /// Per-sender avatar fills; see [`AVATAR`].
    pub avatar: [Color32; 8],
    /// The sender initial drawn on an avatar.
    pub avatar_text: Color32,
}

impl ColorScheme {
    /// Defaults for the light theme.
    pub const fn light() -> Self {
        Self {
            name: "Kimo Light",
            dark: false,
            panel: Color32::from_rgb(237, 238, 241), // #EDEEF1
            content: Color32::from_rgb(250, 250, 251), // #FAFAFB
            window: Color32::from_rgb(255, 255, 255), // #FFFFFF
            card: Color32::from_rgb(241, 242, 245),  // #F1F2F5
            // Dark enough to read as a hover on both the near-white content
            // surface and the gray chrome the navigation rows sit on.
            hover: Color32::from_rgb(225, 228, 234), // #E1E4EA
            input: Color32::from_rgb(255, 255, 255), // #FFFFFF
            code: Color32::from_rgb(238, 239, 243),  // #EEEFF3
            border: Color32::from_rgb(215, 218, 225), // #D7DAE1
            row_edge: Color32::from_rgb(229, 231, 237), // #E5E7ED
            row_edge_top: Color32::from_rgb(240, 241, 245), // #F0F1F5
            text: Color32::from_rgb(30, 34, 40),     // #1E2228
            // Dark enough for AA on the gray chrome as well as on the
            // near-white content surface.
            text_weak: Color32::from_rgb(95, 102, 115), // #5F6673
            on_danger: Color32::from_rgb(255, 255, 255), // #FFFFFF
            accent: Color32::from_rgb(106, 69, 208),    // #6A45D0
            on_accent: Color32::from_rgb(255, 255, 255), // #FFFFFF
            selection: Color32::from_rgb(220, 224, 248), // #DCE0F8
            flag: Color32::from_rgb(255, 243, 219),     // #FFF3DB
            link: Color32::from_rgb(11, 87, 208),       // #0B57D0
            warn: Color32::from_rgb(154, 87, 0),        // #9A5700
            danger: Color32::from_rgb(198, 40, 40),     // #C62828
            danger_hover: Color32::from_rgb(232, 17, 35), // #E81123
            avatar: AVATAR,
            avatar_text: Color32::from_rgb(255, 255, 255), // #FFFFFF
        }
    }

    /// Defaults for the dark theme.
    pub const fn dark() -> Self {
        Self {
            name: "Kimo Dark",
            dark: true,
            panel: Color32::from_rgb(26, 28, 33), // #1A1C21
            // Lifted off the chrome, like the light scheme's near-white: the
            // card, its gutter and the list's hairlines are the step between
            // these two slots, so a content equal to the panel is a reading
            // pane with no card in it.
            content: Color32::from_rgb(32, 35, 41),  // #202329
            window: Color32::from_rgb(34, 37, 43),   // #22252B
            card: Color32::from_rgb(36, 39, 46),     // #24272E
            hover: Color32::from_rgb(46, 51, 60),    // #2E333C
            input: Color32::from_rgb(20, 22, 26),    // #14161A
            code: Color32::from_rgb(43, 48, 56),     // #2B3038
            border: Color32::from_rgb(54, 60, 70),   // #363C46
            row_edge: Color32::from_rgb(44, 48, 56), // #2C3038
            row_edge_top: Color32::from_rgb(38, 42, 49), // #262A31
            text: Color32::from_rgb(230, 232, 236),  // #E6E8EC
            text_weak: Color32::from_rgb(154, 161, 172), // #9AA1AC
            on_danger: Color32::from_rgb(255, 255, 255), // #FFFFFF
            accent: Color32::from_rgb(139, 123, 240), // #8B7BF0
            on_accent: Color32::from_rgb(255, 255, 255), // #FFFFFF
            selection: Color32::from_rgb(58, 53, 104), // #3A3568
            flag: Color32::from_rgb(62, 51, 29),     // #3E331D
            link: Color32::from_rgb(127, 168, 255),  // #7FA8FF
            warn: Color32::from_rgb(224, 163, 58),   // #E0A33A
            danger: Color32::from_rgb(255, 107, 107), // #FF6B6B
            danger_hover: Color32::from_rgb(232, 17, 35), // #E81123
            avatar: AVATAR,
            avatar_text: Color32::from_rgb(255, 255, 255), // #FFFFFF
        }
    }

    /// The scheme as egui's `Visuals`, which is what every stock widget draws
    /// with. Text keeps one color in all widget states so that labels stay
    /// readable on the hover and selection fills.
    pub fn visuals(self) -> egui::Visuals {
        let mut visuals = if self.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        let text = egui::Stroke::new(1.0_f32, self.text);
        let border = egui::Stroke::new(1.0_f32, self.border);

        visuals.dark_mode = self.dark;
        visuals.panel_fill = self.panel;
        visuals.window_fill = self.window;
        visuals.window_stroke = border;
        visuals.faint_bg_color = self.card;
        visuals.text_edit_bg_color = Some(self.input);
        visuals.code_bg_color = self.code;
        visuals.hyperlink_color = self.link;
        visuals.warn_fg_color = self.warn;
        visuals.error_fg_color = self.danger;
        visuals.selection.bg_fill = self.selection;
        // egui draws the text of a selected item in this stroke's color.
        visuals.selection.stroke = text;

        for widget in [
            &mut visuals.widgets.noninteractive,
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
            &mut visuals.widgets.open,
        ] {
            widget.fg_stroke = text;
            widget.bg_stroke = border;
        }
        // Faded-out widgets fade towards the panel they sit on; that same mix
        // is what egui derives `weak_text_color` from.
        visuals.widgets.noninteractive.bg_fill = self.panel;
        visuals.widgets.noninteractive.weak_bg_fill = self.panel;
        // Button frames: card, then hover, then pressed/selected.
        visuals.widgets.inactive.bg_fill = self.card;
        visuals.widgets.inactive.weak_bg_fill = self.card;
        visuals.widgets.hovered.bg_fill = self.hover;
        visuals.widgets.hovered.weak_bg_fill = self.hover;
        for widget in [&mut visuals.widgets.active, &mut visuals.widgets.open] {
            widget.bg_fill = self.selection;
            widget.weak_bg_fill = self.selection;
            widget.bg_stroke = egui::Stroke::new(1.0_f32, self.accent);
        }
        visuals
    }

    /// Labeled colors for the Appearance page preview, in the order the fields
    /// are declared.
    pub fn swatches(self) -> [(&'static str, Color32); 20] {
        [
            ("Panel", self.panel),
            ("Content", self.content),
            ("Window", self.window),
            ("Card", self.card),
            ("Hover", self.hover),
            ("Input", self.input),
            ("Code", self.code),
            ("Border", self.border),
            ("Mail edge", self.row_edge),
            ("Mail edge top", self.row_edge_top),
            ("Flag", self.flag),
            ("Text", self.text),
            ("Text weak", self.text_weak),
            ("Accent", self.accent),
            ("On accent", self.on_accent),
            ("Selection", self.selection),
            ("Link", self.link),
            ("Warn", self.warn),
            ("Danger", self.danger),
            ("Danger hover", self.danger_hover),
        ]
    }
}

/// The light palette; see [`ColorScheme::light`].
pub const LIGHT: ColorScheme = ColorScheme::light();
/// The dark palette; see [`ColorScheme::dark`].
pub const DARK: ColorScheme = ColorScheme::dark();

fn scheme_id() -> egui::Id {
    egui::Id::new("kimo-color-scheme")
}

/// Apply a scheme to the context: egui's visuals for the stock widgets, and
/// the scheme itself for everything the app paints by hand ([`of`]).
///
/// egui 0.36 keeps one style per theme and picks between them by a preference,
/// so *both* styles are filled in — a stock widget must never come out in
/// egui's own palette, whichever theme egui thinks it is in — and the
/// preference follows this module's scheme, which resolves "system" against
/// the desktop itself rather than letting egui ask a second time (`set_theme`
/// is only called when it would change, so this stays cheap per frame).
pub fn install(ctx: &egui::Context, scheme: ColorScheme) {
    ctx.set_visuals_of(egui::Theme::Light, LIGHT.visuals());
    ctx.set_visuals_of(egui::Theme::Dark, DARK.visuals());
    let wanted = if scheme.dark {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    };
    if ctx.options(|options| options.theme_preference) != wanted {
        ctx.set_theme(wanted);
    }
    ctx.data_mut(|data| data.insert_temp(scheme_id(), scheme));
}

/// The scheme installed for this frame, or the dark default before the first
/// [`install`] (nothing draws that early, but painting helpers must not care).
pub fn of(ctx: &egui::Context) -> ColorScheme {
    ctx.data(|data| data.get_temp(scheme_id())).unwrap_or(DARK)
}

/// `#RRGGBB`, for the Appearance preview.
pub fn hex(color: Color32) -> String {
    format!("#{:02X}{:02X}{:02X}", color.r(), color.g(), color.b())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance of an opaque color.
    fn luminance(color: Color32) -> f32 {
        let channel = |v: u8| {
            let s = v as f32 / 255.0;
            if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }

    /// WCAG contrast ratio: 1.0 for two identical colors, 21.0 for black on
    /// white. 4.5 is the floor for body text, 3.0 for UI components.
    fn contrast(a: Color32, b: Color32) -> f32 {
        let (first, second) = (luminance(a), luminance(b));
        let (hi, lo) = if first > second {
            (first, second)
        } else {
            (second, first)
        };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn theme_parses_known_values_and_defaults_to_system() {
        assert_eq!(Theme::parse("dark"), Theme::Dark);
        assert_eq!(Theme::parse("light"), Theme::Light);
        assert_eq!(Theme::parse("nonsense"), Theme::System);
        assert_eq!(Theme::parse(""), Theme::System);
    }

    #[test]
    fn theme_resolves_to_a_scheme() {
        assert_eq!(Theme::Light.scheme(true), LIGHT);
        assert_eq!(Theme::Dark.scheme(false), DARK);
        assert_eq!(Theme::System.scheme(false), LIGHT);
        assert_eq!(Theme::System.scheme(true), DARK);
        assert_eq!(Theme::ALL.len(), 3);
        assert!(LIGHT.name != DARK.name);
    }

    #[test]
    fn every_swatch_is_opaque_and_named_once() {
        let names: Vec<&str> = DARK.swatches().iter().map(|(name, _)| *name).collect();
        let light_names: Vec<&str> = LIGHT.swatches().iter().map(|(name, _)| *name).collect();
        assert_eq!(names, light_names, "both schemes list the same slots");
        for (index, name) in names.iter().enumerate() {
            assert!(!names[..index].contains(name), "{name} is listed twice");
        }
        for (name, color) in DARK.swatches().into_iter().chain(LIGHT.swatches()) {
            assert!(color.is_opaque(), "{name} must be opaque");
        }
    }

    #[test]
    fn surfaces_are_lighter_in_the_light_scheme() {
        for (name, light, dark) in [
            ("panel", LIGHT.panel, DARK.panel),
            ("content", LIGHT.content, DARK.content),
            ("window", LIGHT.window, DARK.window),
            ("card", LIGHT.card, DARK.card),
            ("hover", LIGHT.hover, DARK.hover),
            ("input", LIGHT.input, DARK.input),
            ("code", LIGHT.code, DARK.code),
            ("border", LIGHT.border, DARK.border),
            ("row edge", LIGHT.row_edge, DARK.row_edge),
            ("row edge top", LIGHT.row_edge_top, DARK.row_edge_top),
        ] {
            assert!(
                luminance(light) > luminance(dark),
                "{name} must be lighter in the light scheme"
            );
        }
        // Body text goes the other way and so contrasts with its own panel.
        assert!(luminance(LIGHT.text) < luminance(LIGHT.panel));
        assert!(luminance(DARK.text) > luminance(DARK.panel));
        assert!(luminance(LIGHT.panel) > luminance(LIGHT.hover));
    }

    /// The card, its gutter and the list's hairlines are *the step between the
    /// two surfaces* and nothing else, so the working surface has to stay
    /// lighter than the chrome in both schemes: the dark scheme once set them
    /// to the same color, which is a reading pane with no card in it.
    #[test]
    fn the_working_surface_is_lighter_than_the_chrome() {
        for (name, scheme) in [("light", LIGHT), ("dark", DARK)] {
            let step = luminance(scheme.content) / luminance(scheme.panel);
            assert!(step >= 1.1, "{name}: content is only {step:.2}x the panel");
        }
    }

    /// A mail row's edges are separations, not rules: every one of them has to
    /// be *fainter* against the list than a hairline is, and the top edge — the
    /// one facing the mail above — fainter than the rest, because that pair of
    /// faint lines is the whole of the space between two mails.
    #[test]
    fn the_mail_rows_are_separated_by_a_softer_line_than_a_hairline() {
        for (name, scheme) in [("light", LIGHT), ("dark", DARK)] {
            let hairline = contrast(scheme.border, scheme.content);
            let edge = contrast(scheme.row_edge, scheme.content);
            let top = contrast(scheme.row_edge_top, scheme.content);
            assert!(
                edge < hairline,
                "{name}: a row edge is {edge:.2}:1 against the list, a hairline {hairline:.2}:1"
            );
            assert!(
                top < edge,
                "{name}: a row's top edge is {top:.2}:1 against the list, its sides {edge:.2}:1"
            );
        }
    }

    #[test]
    fn foreground_colors_are_readable_on_their_panel() {
        for (name, scheme) in [("light", LIGHT), ("dark", DARK)] {
            for (surface, background) in [("panel", scheme.panel), ("content", scheme.content)] {
                for (slot, color) in [
                    ("text", scheme.text),
                    ("text weak", scheme.text_weak),
                    ("link", scheme.link),
                    ("warn", scheme.warn),
                    ("danger", scheme.danger),
                ] {
                    let ratio = contrast(color, background);
                    assert!(ratio >= 4.5, "{name}: {slot} on {surface} is {ratio:.1}:1");
                }
            }
            // The accent is a fill and focus color, not text.
            for (surface, background) in [("panel", scheme.panel), ("content", scheme.content)] {
                let accent = contrast(scheme.accent, background);
                assert!(
                    accent >= 3.0,
                    "{name}: accent on {surface} is {accent:.1}:1"
                );
            }
            // Weak text is dimmer than body text, but both stay readable.
            assert!(contrast(scheme.text, scheme.panel) > contrast(scheme.text_weak, scheme.panel));
            for (slot, fill) in [
                ("selection", scheme.selection),
                ("hover", scheme.hover),
                // A flagged mail's row is the one fill a mail keeps for as long
                // as the flag stands, so its text has to survive it too.
                ("flag", scheme.flag),
            ] {
                let ratio = contrast(scheme.text, fill);
                assert!(ratio >= 4.5, "{name}: text on {slot} is {ratio:.1}:1");
            }
            // The close button's glyph has to survive its red.
            let glyph = contrast(scheme.on_danger, scheme.danger_hover);
            assert!(
                glyph >= 4.5,
                "{name}: on_danger on danger_hover is {glyph:.1}:1"
            );
            // The check in a checked box is a UI mark rather than text, so it
            // answers to the graphic floor.
            let mark = contrast(scheme.on_accent, scheme.accent);
            assert!(mark >= 3.0, "{name}: on_accent on accent is {mark:.1}:1");
        }
    }

    #[test]
    fn avatar_palette_is_shared_and_distinct() {
        assert_eq!(LIGHT.avatar, DARK.avatar, "a sender keeps its color");
        assert_eq!(LIGHT.avatar_text, DARK.avatar_text);
        for (index, color) in DARK.avatar.iter().enumerate() {
            assert!(color.is_opaque(), "avatar {index} must be opaque");
            assert!(
                !DARK.avatar[..index].contains(color),
                "avatar {index} repeats a color"
            );
        }
    }

    #[test]
    fn visuals_follow_the_scheme() {
        let light = LIGHT.visuals();
        let dark = DARK.visuals();
        assert!(!light.dark_mode);
        assert!(dark.dark_mode);
        for visuals in [&light, &dark] {
            let scheme = if visuals.dark_mode { DARK } else { LIGHT };
            assert_eq!(visuals.panel_fill, scheme.panel);
            assert_eq!(visuals.window_fill, scheme.window);
            assert_eq!(visuals.faint_bg_color, scheme.card);
            assert_eq!(visuals.text_edit_bg_color, Some(scheme.input));
            assert_eq!(visuals.code_bg_color, scheme.code);
            assert_eq!(visuals.hyperlink_color, scheme.link);
            assert_eq!(visuals.warn_fg_color, scheme.warn);
            assert_eq!(visuals.error_fg_color, scheme.danger);
            assert_eq!(visuals.selection.bg_fill, scheme.selection);
            assert_eq!(visuals.selection.stroke.color, scheme.text);
            assert_eq!(visuals.text_color(), scheme.text);
            assert_eq!(visuals.strong_text_color(), scheme.text);
            assert_eq!(
                visuals.widgets.noninteractive.bg_stroke.color,
                scheme.border
            );
            assert_eq!(visuals.widgets.inactive.weak_bg_fill, scheme.card);
            assert_eq!(visuals.widgets.hovered.bg_fill, scheme.hover);
            assert_eq!(visuals.widgets.active.bg_fill, scheme.selection);
            assert_eq!(visuals.widgets.active.bg_stroke.color, scheme.accent);
        }
    }

    #[test]
    fn install_hands_the_scheme_back_to_the_painters() {
        let ctx = egui::Context::default();
        assert_eq!(
            of(&ctx),
            DARK,
            "dark is the fallback before the first frame"
        );
        install(&ctx, LIGHT);
        assert_eq!(of(&ctx), LIGHT);
        assert_eq!(ctx.style_of(ctx.theme()).visuals.panel_fill, LIGHT.panel);
    }

    #[test]
    fn hex_formats_rgb() {
        assert_eq!(hex(Color32::from_rgb(0x1a, 0x2b, 0x3c)), "#1A2B3C");
        assert_eq!(hex(Color32::WHITE), "#FFFFFF");
    }
}
