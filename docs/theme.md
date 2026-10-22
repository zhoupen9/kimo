# Theme — colors and fonts

Read when touching `src/theme.rs`, `src/fonts.rs`, or adding a color to a widget.

## Colors

- Colors live in one place, `src/theme.rs`. `Theme` (system/light/dark — the
  only part that is persisted) resolves to a `ColorScheme`, and both defaults
  are filled in slot by slot, `ColorScheme::light()` and
  `ColorScheme::dark()`: surfaces (panel — the chrome; content — the message
  list and the reading pane's card; window, card, hover, input, code, border;
  `flag`, the warning tint a flagged mail's row in the list takes;
  and `row_edge` / `row_edge_top`, the two faint lines a mail row is drawn
  with, fainter than a hairline and the top one fainter than the sides),
  text and weak text, accent / selection / link, warn / danger / danger_hover,
  and the avatar palette — shared by both themes on purpose, so that a sender
  keeps its color when the theme changes. `content` is the lighter of the two
  surfaces in both schemes, and the step between it and `panel` is the *only*
  thing that draws the card, its gutter and the list's hairlines — the dark
  scheme used to set both slots to the same color, which is a reading pane with
  no card in it, so `theme.rs`'s tests pin a minimum ratio between them.
  `ColorScheme::visuals()` maps the scheme onto egui's `Visuals` for every
  stock widget, while `theme::install` also parks the scheme in the context,
  where the hand-painted UI (title bar, folder rows, message cards, avatars,
  spinners) reads it back through `theme::of`. So a color is added to the
  scheme and nowhere else — no literals in `app.rs` — and stock-widget slots
  stay reachable through egui itself (`ui.visuals().error_fg_color` is the
  scheme's `danger`, `widgets.active.bg_fill` its `selection`). egui derives
  its own weak/strong text variants by tinting the body color towards the
  panel, so `RichText::weak()` is close to, but not the same as, the scheme's
  `text_weak`; the folder and message rows use the latter. Preferences →
  Appearance previews the resolved scheme (`ColorScheme::swatches`,
  `theme::hex`), and the one slot of it a reader can change is the accent:
  `theme::Accent::ALL` is the six choices, the page draws them as chips
  (`accent_chip`, the one in force ringed), `settings.json` stores the
  key, and `ColorScheme::with_accent` puts the pair into the scheme the
  theme resolves to — `Accent::KIMO` is the pair the schemes already hold,
  so the default changes nothing. An accent carries the colour *and* the
  tint selected rows are filled with, once per theme, because a violet that
  reads on near-white paper is not the violet that reads on a dark surface.
  Everything else about the palette is still not editable, which is why
  nothing else about it is written back to `settings.json`. `theme.rs`'s tests pin the
  contrast floors **over every accent of `Accent::ALL`, in both themes** (body
  text ≥ 4.5:1 on its panel *and* on its content, accent
  ≥ 3:1, text on the selection, hover and flag fills ≥ 4.5:1, weak text on a
  selected row ≥ 4:1, the close glyph on its
  red ≥ 4.5:1, both schemes' surfaces on opposite sides of the brightness
  scale, and the mail rows' edges fainter against the list than a hairline,
  with the top edge fainter than the sides); keep them green when you touch a
  color.
  **A focused field's box is the accent** (`Field`, app.rs). egui draws a text
  edit's frame from `Visuals::selection.stroke` while the keyboard is in it —
  the same slot as the colour of *selected text* — so a field that took that
  line for its outline would recolour every selection in the app. Instead the
  field is added with a frame that draws nothing (`field_frame`, whose margin is
  the text's own) and Kimo paints the box itself: the theme's input fill under
  the text, and over it the border — or the accent, with the keyboard in the
  field. The writing area is the same box drawn by `compose_body`, around the
  room the card gives it rather than around the field's own height. The title
  bar's search box is the one field not drawn this way: it is a non-interactive
  look-alike behind the search popup, with the glass fill and radius of its own.

## Fonts

- Fonts are discovered, never fetched through fontconfig (hard rule 1):
  `fonts::scan()` walks the usual directories and reads each face's `name`,
  `OS/2` and `post` tables — a few KiB per face — instead of reading the font
  file whole. Reading the files took ~2.4 GB of I/O and left the allocator
  holding ~50 MB afterwards on the development machine (four pan-CJK
  collections are ~90 MB on their own); the table reads are ~4 MB. The scan
  runs as `Job::ScanFonts` on a worker thread and arrives as
  `Event::FontsScanned`; only the single-file read that installs a picked
  family happens on the UI thread. Name records are decoded exactly as
  ttf-parser decodes them (Unicode platform, Windows symbol/BMP encodings),
  and a test compares the reader with a full ttf-parser parse on the installed
  fonts, because the picker's names and chosen face are what `settings.json`
  stores. Families are keyed by their *typographic* family name, and each one
  keeps its plainest face (regular weight, non-italic) so "DejaVu Sans" does
  not resolve to the ExtraLight file. `fonts::install` also binds the named
  families `kimo_ui`/`kimo_mono` — epaint panics on an unbound named family —
  which the Appearance page previews through. Blitz keeps its own curated font
  set (`blitz_render::load_fonts`), matched by family name rather than file
  name (see `docs/html-mail.md`); the Preferences choice applies to the egui UI
  and to monospace text in the built-in HTML renderer. The CJK fallback is a
  pan-CJK collection (Noto Sans CJK), whose faces cover the same characters
  but not the same *forms*: the face therefore follows the locale
  (`CjkScript`: zh-CN → SC, zh-TW/HK → TC, ja → JP, ko → KR, anything else
  → SC), because taking the collection's first face means the Japanese one —
  which is how Chinese mail came out with Japanese glyphs for 复 and friends.
  Do not go back to an index-free `read(path)`.

- **The face can be picked** (`cjk_font`, see [preferences.md](preferences.md)).
  The pick replaces the automatic file and the automatic path stays untouched
  when nothing is picked. It is a *trade*, measured on a box that had all of
  these (`cmap` coverage, Han URO / kana / Hangul / Bopomofo):

  | Family | File | Han | kana | Hangul | Bopomofo |
  | --- | --- | --- | --- | --- | --- |
  | Noto Sans CJK, each SC/TC/JP/KR face | 18.6 MB | 100 % | 97–100 % | 100 % | 100 % |
  | WenQuanYi Zen Hei | 16 MB | 100 % | 99 % | 100 % | 100 % |
  | WenQuanYi Micro Hei | 5.0 MB | 100 %* | 94 % | 100 % | **0 %** |
  | a Simplified-Chinese-only font (MS YaHei here) | 15 MB | 100 % | 97 % | **0 %** | 100 % |

  *Han URO only: the 5 MB file has no Ext-A block, which is the "no rare Han"
  the size buys. Only reading `cmap` can know this — the scanner deliberately
  does not — so the picker probes the picked file once and says what it lacks
  (`fonts::missing_cjk_samples`), instead of substituting silently. A Korean
  mail in a Chinese-only face is tofu, not Korean glyphs; that is the failure
  mode the warning exists for.

- **Rasterisation follows the display, not the theme.** egui antialiases every
  glyph with coverage, and by default also hints it and keeps up to four
  horizontal subpixel offsets of its bitmap. Both pay at one pixel per point,
  where a stem is one or two pixels wide and snapping it to the grid is what
  keeps 11 pt text crisp. At two pixels per point the same row is 20–40 px
  tall: hinting then only bends the designer's outline — a stem that should be
  2.5 px wide is pushed to 2 or 3, and one stroke of a letter comes out lighter
  than the next — and the quarter-pixel bitmaps differ where no eye can see it,
  for up to four atlas entries per glyph. `fonts::tune_text` switches both off
  at two pixels per point and above; `theme::install` calls it every frame with
  the context's `pixels_per_point`, so the rule follows the window to another
  monitor. The alternative a FreeType stack would use — `hintslight` — is not
  reachable here: skrifa's auto-hinter renders the `light` and `normal` targets
  identically (verified byte for byte), and a font that ships TrueType
  instructions ignores the smooth mode. The real choice is the hinted outline
  or the drawn one, and at 2× the drawn one is what GTK and macOS show. At 1×
  the hinting stays on: that is where it earns its keep.
