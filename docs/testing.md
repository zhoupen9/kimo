# Testing — looking at the UI and at the renderers

Read before claiming a UI or renderer change works, or when timing one.

## Plugins before the test suite

The host's tests load the real provider and extension modules through the ABI
with `include_bytes!` from `plugins/<name>/<name>.wasm`. Those binaries are
build output and are ignored by git, so a fresh worktree has to build and copy
them before `cargo test` (or `cargo clippy --all-targets`) will even compile:

```sh
(cd plugins/gmail && cargo build --release --target wasm32-unknown-unknown)
cp plugins/gmail/target/wasm32-unknown-unknown/release/kimo_plugin_gmail.wasm \
   plugins/gmail/gmail.wasm

(cd plugins/outlook && cargo build --release --target wasm32-unknown-unknown)
cp plugins/outlook/target/wasm32-unknown-unknown/release/kimo_plugin_outlook.wasm \
   plugins/outlook/outlook.wasm

(cd plugins/sender-groups && cargo build --release --target wasm32-unknown-unknown)
cp plugins/sender-groups/target/wasm32-unknown-unknown/release/kimo_plugin_sender_groups.wasm \
   plugins/sender-groups/sender-groups.wasm
```

## Looking at the UI

- When touching the UI, actually run `cargo run` and click through it; the
  layout has no golden tests. `blitz_render` has a developer tool for
  eyeballing fidelity without the GUI:
  `KIMO_RENDER_HTML=mail.html cargo test -- --ignored dump_render` writes
  `mail.png` next to the input. `dump_ui` (`src/app/tests/dump.rs`) is the same idea for the app's
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
  frames rather than bodies — including what a splitter drag costs —
  `src/app/tests/bench.rs` has `bench_frame_cost`, which drives the real app
  headlessly at a fixed splitter position and while it is being dragged:
  `cargo test --release -- --ignored --nocapture bench_frame_cost`. Its
  companion `bench_nav_slide` measures those frames through the navigation
  column's collapse and its slide back open, which grows the reading pane's
  width the way a drag does
  (`cargo test --release -- --ignored --nocapture bench_nav_slide`).
