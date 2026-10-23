# Testing — looking at the UI and at the renderers

Read before claiming a UI or renderer change works, or when timing one.

## Looking at the UI

- When touching the UI, actually run `cargo run` and click through it; the
  layout has no golden tests. `blitz_render` has a developer tool for
  eyeballing fidelity without the GUI:
  `KIMO_RENDER_HTML=mail.html cargo test -- --ignored dump_render` writes
  `mail.png` next to the input. `dump_ui` is the same idea for the app's
  own frames and needs no compositor at all:
  `cargo test -- --ignored --nocapture dump_ui` drives the clicks
  itself and rasterizes the frames — the columns, the card, the compose form in
  both its containers, the search popup, the label popup, Preferences → General,
  Appearance and About — straight from egui's own triangles into
  `target/ui-*.png`
  (`KIMO_UI_DUMP` picks another directory), which is how a layout is looked at
  on a machine whose display is somebody else's. Its frames have to go through
  `collect_frame` or a glyph the frame was the first to draw comes out blank:
  egui sends the font atlas whole once and patches after that. Looking at a
  picture is not running the app — click through the real one before calling a
  change done. For cost rather than fidelity, use
  `render_bench` (test-only, asserts nothing):
  `cargo test --release -- --ignored --nocapture bench_render` times both
  renderers on a short mail, a newsletter-shaped fixture and — with
  `KIMO_BENCH_DB=~/.local/share/kimo/mail.db` — the median-sized and largest
  HTML bodies in the database. Note what the numbers mean: the built-in
  renderer lays the body out on *every* frame the reading pane is visible —
  there is no cache of the widget tree — but it parses it once and keeps the
  width it settled at (`BodyCache`), so a frame costs the layout, not the parse
  and not a reflow per pixel of a splitter drag. Blitz pays parse, layout and
  rasterization once per (message, pane width), on a worker thread. For whole
  frames rather than bodies — including what a splitter drag costs — `app.rs`
  has `bench_frame_cost`, which drives the real app headlessly at a fixed
  splitter position and while it is being dragged:
  `cargo test --release -- --ignored --nocapture bench_frame_cost`. Its
  companion `bench_nav_slide` measures those frames through the navigation
  column's collapse and its slide back open, which grows the reading pane's
  width the way a drag does
  (`cargo test --release -- --ignored --nocapture bench_nav_slide`).
