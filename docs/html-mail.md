# HTML mail — the two renderers

Read when touching `src/html.rs`, `src/css.rs`, `src/blitz_render.rs`, or a Blitz build feature.

- **HTML mail:** two renderers, switched in Preferences → Appearance and
  remembered in `settings.json` (`html_renderer`).
  *Simple* (the default) parses with html5ever (via `scraper`) and draws
  egui widgets in `src/html.rs` with a small CSS subset (`src/css.rs`):
  theme-following, selectable, clickable, no layout fidelity.  The mail's own
  colours are read as CSS (`csscolorparser`) and adapted to the theme in force
  (`palette`, OKLCH) before they are drawn — see the colors section in
  [theme.md](theme.md).  *Blitz* feeds
  the same HTML to the pure-Rust Blitz browser engine (stylo/taffy/
  vello_cpu) in `src/blitz_render.rs` and blits the rasterized page — close
  to what the sender designed, but a static image with clickable link boxes.
  Remote images and stylesheets load **by default** in both (see the
  remote-resources decision in [architecture.md](architecture.md)). There is still no JavaScript.

## The Blitz render job

- The Blitz renderer is a `Job::RenderHtml` on a worker thread: it parses,
  styles, lays out and rasterizes the body, resolving every sub-resource
  through `blitz_render::MailNet` (a blocking `NetProvider` that reuses the
  on-disk resource cache, and understands `data:` and `cid:` URLs). The page
  comes back as `Event::HtmlRendered` with premultiplied RGBA tiles and link
  boxes, which `app.rs` uploads as textures. Pages are keyed by mail +
  quantized *width* — never the pane height, which only feeds Blitz's viewport
  (`vh` and friends): keying on it re-rasterized an identical page on every
  window resize. At most two pages are cached; a cached page is reused, scaled
  down and never up, while the pane stays within `BLITZ_REUSE_RATIO` of the
  width it was laid out for; and a render starts only once the geometry has
  held still for `GEOMETRY_SETTLE`, so a splitter drag or an animated window
  resize queues one render at the end instead of one per frame (the built-in
  renderer waits the same time — see the reading pane in [ui-mail.md](ui-mail.md)).
  Every
  request takes a fresh `blitz_render::Cancellation` ticket: a render whose
  ticket is stale gives up mid-layout (and between raster slices) and reports
  *nothing*, so a superseded render can never be mistaken for a failure of the
  newer one. Never render Blitz HTML on the UI thread.

## Blitz build and scale

- Blitz is built with `blitz-dom`'s default features **off**: its
  `system-fonts` feature would link fontconfig (a C library, hard rule 1).
  `blitz_render` therefore registers fonts from disk itself (a curated set —
  parsing every installed face took ~46s in a debug build) and maps the CSS
  generic families plus the per-script fallbacks onto them. Keep it that way,
  and keep the curated list in sync with the generic families. Match a face's
  **family name**, read from the face (`fonts::faces`), never the file name:
  a file name is not a family — "DejaVuSansMNerdFont" contains "DejaVuSans",
  and "NotoSerifCJK-SemiBold" contains "Bold" — and matching on it registered
  179 MiB of faces on the development machine instead of 122 MiB, none of the
  difference asked for by any mail. CJK families take only their Regular and
  Bold faces for the same reason.

- Blitz's painter scale must equal the document's viewport scale. Text is
  laid out at the viewport scale but positioned with the painter's scale, so
  `paint_scene(.., scale, ..)` with a scale the document was not resolved for
  draws oversized text that overflows its box — and, because the reading pane
  sizes tiles from the page's own scale, a mismatch also mis-sizes the whole
  page in the pane. To rasterize more cheaply, re-layout via `set_viewport`
  (see `blitz_render::render`) and report the scale you used in `Page::scale`.

- Keep `complex-scripts` on blitz-dom. Without it parley has no CJK/Thai/
  Khmer/Lao segmentation model: ICU logs "No segmentation model for complex
  script" into stderr for every such run and the text is only broken by the
  coarse UAX-14 rules. The dictionaries it enables are already-compiled ICU4X
  data, so the feature costs no new crates.
