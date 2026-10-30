# UI shell — window, title bar and columns

Read when touching `src/main.rs`, `src/gpu.rs`, `src/gpus.rs`,
`src/glass.rs`, `src/app/titlebar.rs`, `src/app/layout.rs`,
`src/app/navigation.rs`, the title bar, or the columns' layout plan.

- **GUI:** egui (immediate mode, pinned to the 0.36 line), driven directly
  through winit + `egui-winit` + `egui-wgpu` instead of through eframe, because
  the floating title bar's blur needs the frame split in two — see the
  egui 0.36 section below.

## egui 0.36, winit and wgpu

- egui is pinned to the `0.36` line, with `egui-winit`, `egui-wgpu` and `wgpu`
  moving in lockstep, and the step from 0.31 was not a version bump but a change
  of shape: 0.36 draws the whole app into one root `Ui` (`KimoApp::ui` — there
  is no `Context`-level `update` any more), the side panels are `egui::Panel::left`/`right` shown *onto* that `Ui`
  (with `min_size`/`default_size`, not `min_width`/`default_width`), the
  context's `screen_rect`/`available_rect` are `viewport_rect`/`content_rect`
  (the message list's own rect is taken from `mail_list`, which records it),
  `Ui::allocate_new_ui` is `Ui::scope_builder`, a galley needs
  `Context::fonts_mut`, and the styles are *per theme*: `theme::install` fills
  in both of them and sets the preference from Kimo's own system detection, so
  a stock widget is never drawn in egui's palette. The layout tests run their
  frame through `run_frame`, because epaint panics on a dropped
  `TexturesDelta`. The integration is `src/gpu.rs` + `src/main.rs`, not
  eframe: eframe owns the frame's render pass, and the glass needs it split in
  two (see the frosted glass section below). `main.rs` is a winit
  `ApplicationHandler` — window, `egui_winit::State` for input, egui's repaint
  callback turned into a `ControlFlow::WaitUntil` timer (a deadline that has
  not come round yet must be *left in place*: a loop that consumes it sleeps
  through the frame egui asked for), and
  `egui_winit::process_viewport_commands` for the commands the title bar sends
  (drag, minimize, maximize), with two catches that are the loop's own to
  handle: a close is only *recorded* by it, as a `ViewportEvent::Close` in the
  `ViewportInfo` it is handed (in eframe that recording is the app's chance to
  refuse the close, via `CancelClose`), so the loop reads it and ends itself —
  unless the title bar sent `Visible(false)` instead, because close-to-tray is
  on — and the compositor's own `WindowEvent::CloseRequested` takes the same
  hide-or-quit decision. The tray's Quit does not send a close at all: it sets
  the loop's quit request, which close-to-tray must not be able to swallow.
  That info has to be refreshed from the window every frame
  (`egui_winit::update_viewport_info`) and handed to egui as
  `RawInput::viewports`, because `egui-winit` itself fills in nothing
  but the scale factor: without it `i.viewport().maximized`, which draws the
  title bar's maximize/restore glyph, is always `None` and the maximize button
  can never restore. The renderer is wgpu, and its 0.36-era API
  is young: `InstanceDescriptor` is built by `new_with_display_handle`,
  presenting is `Queue::present`, pipeline layouts take
  `bind_group_layouts: &[Some(&layout)]` and `immediate_size`, passes and
  pipelines take `multiview_mask`, and a `TexturesDelta` must be emptied
  (`set.clear()` as it is uploaded, `free` when the frame is on its way)
  because epaint panics on one that is dropped unapplied. Keep `egui`,
  `egui-winit`, `egui-wgpu` and `wgpu` in lockstep when bumping, and read the
  egui CHANGELOG before doing it.

- The instance asks for **one backend at a time**: Vulkan first, and GL only
  when Vulkan yields no adapter or no surface (`Gpu::new`). `Backends::all()`
  brings up the GL backend as well — EGL and GBM initialised, Mesa's GL driver
  and the compiled shader cache resident — beside the one device that draws;
  measured on the machine in [fix.md](fix.md), that was about 21 MB of a window
  that used none of it. The GL attempt stays because a machine whose Vulkan
  loader is broken but whose GL driver works must still start. Which adapter
  answered is logged once at start-up (`kimo: adapter "…" (DiscreteGpu,
  Vulkan backend, … / …)`): it decides how much of the machine is resident
  behind the window, and without the line that can only be guessed from
  `/proc/<pid>/smaps`.

- **The GPU is picked, and so is how much of the Vulkan stack is opened**
  (Preferences → Graphics, `src/gpus.rs`). The loader opens every installed ICD
  as it creates the instance, so on a machine with two cards the drivers of the
  card that will not draw are resident as well — on the box measured in
  [fix.md](fix.md), most of rows 1–2. The pick is saved as the
  card's PCI address and read in `main` before the event loop: `gpus::apply`
  sets `VK_DRIVER_FILES` (and the old `VK_ICD_FILENAMES` name) to the manifests
  whose library belongs to that card's vendor, so the loader opens nothing else.
  `Gpu::new` then takes the adapter whose PCI bus id (`VK_EXT_pci_bus_info`,
  which wgpu reports as `device_pci_bus_id`) — or vendor and device ids — are
  that card's. Every way a pick can fail falls back to loading every driver:
  absent, malformed, the card unplugged, no installed driver for it, or a
  session that set `VK_DRIVER_FILES` itself. A preference must never be the
  reason there is no GPU at all. Should the picked card's adapter turn out to be
  unusable, `Gpu::new` falls back to wgpu's own choice and then to GL — the
  other cards' drivers cannot be brought back into the process, so that is the
  path a broken driver has always taken. Measured on this box with an empty
  database: every driver 190 MB RSS, the Intel card picked 110 MB, and no
  `libGLX_nvidia`, `libnvidia-*`, `libvulkan_lvp` or `libLLVM` in the
  process.

## The window and its title bar

- The window is undecorated on purpose (`with_decorations(false)` in
  `main.rs`): `titlebar_area()` in `src/app/titlebar.rs` draws the custom title bar — an
  `egui::Area` fixed to the window's top edge and shown *after* the panels, not
  a `TopBottomPanel`, because the bar floats over the columns and the message
  list scrolls under it. At the top of the list it is the solid chrome band it
  has always been; once the list is scrolled it turns to glass — the fill keeps
  `GLASS_ALPHA` of the scheme's `panel`, the backdrop behind it is blurred
  (`src/glass.rs`) and a hairline appears along its bottom edge. One animated
  value (`TITLEBAR_FLOAT_TIME`) drives all three from the columns' scroll
  offsets (`KimoApp::nav_scroll`, `list_scroll` and `mail_scroll`, each captured
  where its scroll area is built), so scrolling any of them back to the top
  docks the bar again rather than snapping it, and a column that is not drawn
  (the navigation in the popup layout, or one the slide has closed) — or has
  nothing to scroll — contributes nothing to it. The room the bar gives its
  own content starts at `TITLEBAR_LEFT_INSET` rather than at the window's
  edge. Inside
  the bar, left to right: the nav collapse
  toggle, the compose button (the one mail action that needs no mail open, so
  it sits beside the toggle rather than with the other actions over the reading
  pane; **those two are the bar's first group**, and the one thing in the bar
  that never gives way — compose is always available, so it is always there; its
  glyph is a page with a pen writing on it
  — `paint_compose_icon`, one of the bar's own 24×24 outlines like the mail
  actions' below — where the icon set's pencil used to be. The page is a
  rounded-rectangle outline whose top-right corner is left *open*, and the pen
  is the straight stroke that crosses exactly there: no stroke of the page ever
  touches it, which is what says the pen is writing on the page rather than
  lying on top of it), the search box (a field with the magnifier inside
  it, one widget,
  whose click opens the search popup), the **filter** cell (a funnel, the
  list's own view control, in the box's group — a cell's gap to its right —
  because it is about what the *column* shows, which is what the box is about
  too; the accent while any view is ticked, and a menu of them under an "All"
  row, `filter_popup`), the **sort** cell (two arrows, one up and one down, right
  after the filter and in the same group: which order the column is in, from
  `sort_popup`, accent-coloured while it is anything but the default), the group
  of mail actions
  (`titlebar_mail_actions`: reply, reply all, forward, label, flag, export,
  delete, one drawing function per glyph, each from a 24×24 outline through
  `glyph_point`), the
  icon-only **Sync now** cell, the
  preferences cog, drag area,
  double-click maximize, and the
  minimize/maximize/close buttons sent as `ViewportCommand`s — minimize is
  `Minimized(true)`, or `Visible(false)` with minimize-to-tray on and a tray
  registered (`minimize_command`), and close is `Close`, or the same
  `Visible(false)` with close-to-tray (`close_command`). On Wayland that
  `Visible(false)` drops and reopens the window instead of unmapping it (see
  [preferences.md](preferences.md)); the loop's `wake` channel is what keeps a
  hidden Kimo alive.
  **The two middle groups are anchored to the columns under them**, not to the
  room left over (`titlebar_anchors`): the box starts at the message list's own
  left edge and the run at the reading pane's, so each sits over the column it
  belongs to and follows that column's splitter — and each is clamped clear of
  the first group, which is what keeps the box off the toggle when the navigation
  is collapsed and the list runs to the window's own edge. The box is the one
  thing the bar gives up as a window narrows: its own width, then its floor
  (`TITLEBAR_SEARCH_FLOOR`), then nothing, with the search popup still a Ctrl+F
  away (`titlebar_search_width`). Its width is planned *first*, and the two
  cells beside it are what fits in what the box leaves of its room — the filter
  and then the sort, in that order, so a bar with room for one of them has the
  filter: a bar with room for the box but not for a cell beside it draws the box
  and neither. The box's width never depends on them, which is what keeps
  resizing a window from flickering the three against each other.
  **The pane's run waits for a mail**: it is drawn only while one is open, or a
  selection is checked (which is what Delete takes), because seven dead glyphs
  over an empty pane said nothing and cluttered the bar. It comes and goes over
  `MAIL_ACTION_FADE_TIME` rather than blinking, and it is given up — not
  squeezed — when the pane has no room for it beside the window controls: it may
  sit over the pane's own edge by up to `MAIL_ACTION_OVERHANG` (the card's
  margin) and never within `MAIL_ACTION_EDGE_GAP` of the controls, and where it
  cannot be this pane's run any more the mail's own card head carries the same
  cells.
  **Sync is not one of the first group's cells**: it is about every account
  rather than about the mail the pane shows, so it stands with the cog and the
  window controls, in a rect of its own (`TitlebarRects::sync`) immediately left
  of the cog — **and it is where the app's sync state is shown**: a spinner in
  place of the glyph while any account is syncing, a dot in the corner when one's
  last sync failed, and the accounts' own words in the tooltip. That is what the
  status strip beside it used to say, one glyph at a time. Folding it into the
  cell a sweep is asked for from is what leaves the pane's run its room at the
  default window width, and it means nothing in the bar moves or reflows when a
  sync starts. Every button in
  the bar is one hand-painted cell (`titlebar_button` + `titlebar_hover`): a
  `TITLEBAR_BUTTON_SIZE` (32 pt) square, centred in the bar's height,
  `TITLEBAR_BUTTON_GAP` (6 pt) from the next, flat at rest and filled under the
  pointer with a small rounded corner (`TITLEBAR_BUTTON_CORNER_RADIUS`, 6 pt)
  rather than the full-height square the bar's buttons used to fill. The left
  group, the mail actions and the window controls
  each set their row's `item_spacing.x` to that gap; the cells the bar places at
  exact x positions rather than in a row — the sync cell, the cog and the
  window controls — are filled through `titlebar_fill` instead. They are not `egui::Button`s for
  that reason — a stock button draws a card-colored frame with a border, which
  in a title bar reads as a widget among window chrome. Don't re-enable system
  decorations.
  The window is transparent (`with_transparent(true)` in `main.rs`) and its
  four corners are cut out of the frame by `gpu::CornerMask`: the last draw of
  a frame scales the four corner squares of the surface by the corner's
  coverage, through a `Zero`/`OneMinusSrcAlpha` blend on the colour *and* the
  alpha so that what is cut stays premultiplied, and the compositor shows what
  is behind the window there. That is why the surface is configured with
  `CompositeAlphaMode::PreMultiplied` — `Auto` resolves to `Opaque`, which
  would cut black corners instead — and why a surface with no alpha mode at all
  keeps its square corners rather than gaining black ones.
  The surface's *color* format is a gamma-space one (`Bgra8Unorm` / `Rgba8Unorm`,
  the first the surface offers), not the sRGB format `get_default_config` picks,
  and that is what makes egui's antialiasing come out right: egui blends
  premultiplied colors, and egui-wgpu's linear-framebuffer shader runs them
  through the sRGB curve *while they are still premultiplied*, so on an sRGB
  target every feathered edge — a rounded hover fill's corners most visibly —
  lands a shade darker than the shape it belongs to. egui-wgpu's own docs ask
  for a gamma-space target for this reason, and the scene texture, the composite
  and the blur all take their format from `config.format`, so nothing below
  converts anything either way. A surface that offers only sRGB formats says so
  once on stderr and keeps the dark edges.
  `WINDOW_CORNER_RADIUS` (8 pt) is also the radius a compositor-side window
  rule has to agree with (niri's `geometry-corner-radius`): without it the
  compositor still assumes a square window and its shadow shows through the
  cut corners.
  The mail actions are the bar's own cells, seven of them in a run: the same
  squares the rest of the bar's buttons are, laid out by a scope of their own
  whose `item_spacing.x` is `TITLEBAR_BUTTON_GAP`. The run's width is one
  function, `titlebar_cells_width`, which places it over the reading pane and
  which the window controls are measured with — so a cell cannot come out one
  size in one group and another size in the next. A glyph is
  `MAIL_ACTION_GLYPH` (18 pt) off its 24×24 outline, which leaves it 7 pt of
  padding inside the cell. The run is deliberately not a framed group: the bar
  stays chrome plus flat icons, and the seven are held together by the clear
  chrome that sets them apart from the search box (`TITLEBAR_GROUP_GAP`) rather
  than by a pill — the same gap, not an elastic one, because the two groups are
  anchored to their columns now. The seven enable and disable
  together, on `mail_action_enabled`: in view mode that is
  `has_selected_message` — the reading pane's own lookup, so a uid the pane no
  longer shows counts as no mail — while select mode leaves Delete alone live
  (see the select-mode bullet in [ui-mail.md](ui-mail.md)), the one of the seven
  that has something to
  do with more than one mail. A disabled cell drops its glyph to
  `weak_text_color`, stops sensing clicks and never fills. All seven are
  wired: reply, reply all and forward open the compose
  (`compose_about`, as the popup the pen opens), label opens the label popup
  (`label_popup`, anchored to the cell), export and delete hand the mail
  to a worker, and flag marks the mail the pane shows (`toggle_flag`). **Flag is
  the one cell with a state of its own**: while the mail carries it, the glyph is
  drawn in the scheme's `warn` — the colour the mail's row and card are tinted
  with — and the hover text says what the click would do now, "Unflag…" rather
  than what a flag is.
  The bar is `TITLEBAR_HEIGHT` (40 pt) tall, its buttons
  `TITLEBAR_BUTTON_SIZE` (32 pt) squares centred in that, and the search box
  fills all but 16 pt of it (`TITLEBAR_SEARCH_HEIGHT`), with a rounder corner
  than the stock widget radius (`SEARCH_BOX_CORNER_RADIUS`); its frame is stock
  `TextEdit` chrome, so the radius is set on the widget visuals in a scope —
  and because the field is deliberately non-interactive, the slot that matters
  is `noninteractive` (egui picks the visuals by `Sense`, not by hover). Its
  *fill* goes to glass with the bar (`GLASS_FIELD_ALPHA`, animated by the same
  value): the one control in the bar with a background of its own would
  otherwise sit on the glass as a solid sticker, so it keeps only part of the
  scheme's `input` and the blurred mail shows through the box the way it
  shows through the bar, with the stroke left to say where the field is. That
  fill has to be set per widget (`TextEdit::background_color`) rather than on
  a widget slot, because the stock frame is drawn from
  `Visuals::text_edit_bg_color` (0.36's name for the slot; it is an `Option`
  now).
  The bar is laid out as *groups*, not as one run of widgets, because what a
  narrow window used to eat was the window controls: the row simply drew past
  the end of the bar, and the search box and the mail cells landed on
  minimize / maximize / close. `titlebar_rects` carves the bar into the room for
  its own content (`content`: everything left of the sync cell) and the fixed
  right end — the sync cell, a gap, the cog, a gap, the three window controls,
  ending `TITLEBAR_RIGHT_MARGIN` short of the window's edge, the count coming
  from `titlebar`'s own button list so the two cannot drift — while the two
  middle groups are placed from the columns below them (`titlebar_anchors`) —
  the box and the filter cell as one group, the pane's run as another — which is
  what makes the bar read as a map of what is under it rather than as a row of
  things that happen to be in it. Those two rects are a cell's own band rather than the
  bar's full height, since a button is 32 pt in a 40 pt bar; `content` is the
  one rect that is the bar's own shape. Every group is
  drawn in a rect of its own, its own `UiBuilder::max_rect` and its own
  `clip_to` for the clip, and that is the rule the rest rests on: a group the bar
  cannot fit is cut off at its edge rather than drawn over its neighbour, and,
  since egui hit-tests a widget through its clip rect, a click past that edge
  belongs to the group next to it instead of to a widget that is not really
  there. `nothing_in_the_title_bar_is_drawn_into_the_window_controls` holds the
  drawing to it by sorting a frame's shapes by the clip they were drawn through,
  which is what tells the bar's right end from a widget that has wandered into
  it — what the old bar did at every width under about 970 pt.
  The box is the one thing given up, and in one order (`titlebar_search_width`,
  pinned by `the_title_bar_gives_its_room_up_in_one_order`): its own width
  while there is room, its floor while the room is short, and nothing below that,
  with the search popup still a Ctrl+F away. Every width is monotone in the room,
  so resizing a window never flickers between two shapes. The pane's run has its
  own rule instead — it is not the room's leftovers, it is the pane's row: it is
  placed at the pane's edge (or over it by no more than `MAIL_ACTION_OVERHANG`),
  and given up when the window controls leave no room for it there, which the
  mail's card head answers for. The plan and the widget also
  have to agree on what "the box's width" means: `TextEdit::desired_width` is the
  *whole* box in 0.36, the margins coming out of it rather than being added to
  it, which is why the rect the widget hands back — the frame, not the inner text
  rect — is used as the box as it is. Re-adding those margins, as the 0.31-era
  code did, puts the magnifier 22 pt left of the box, over the compose cell, and
  stretches the click target over it. The test named
  `the_search_box_is_as_wide_as_it_was_planned` is what keeps the plan's number
  and the widget's the same one.
  **The client draws the resize border too**, because a frame with no decorations
  has no system border to drag: `window_resize_grips` puts eight `RESIZE_GRIP`
  (5 pt) strips in the foreground layer — the four edges and the four corners,
  the corner squares their own grip so an edge cannot take them — and a press on
  one sends `ViewportCommand::BeginResize`, which `egui-winit` turns into
  winit's `drag_resize_window` (`xdg_toplevel.resize` on Wayland,
  `_NET_WM_MOVERESIZE` on X11); the pointer on a strip wears that edge's cursor
  while it is there. The press itself is the trigger rather than egui's drag
  threshold, because the compositor's resize starts from the button that is down
  now. The strips answer over the title bar's own drag stretches — moving the
  window is still the bar's, from `RESIZE_GRIP` down — and a maximized window,
  which has no edge to pull, is given none. Each grip is an area of its own
  rather than one ring around the window, so the columns are shadowed only under
  the pointer.

## The three columns

- **The three columns are sized in one order** (`plan_columns`, read by
  `KimoApp::ui` every frame): the navigation has a fixed width, the message
  list is semi-fixed (a remembered width, capped by the window and by its own
  maximum), and the reading pane is the remainder. The navigation takes its
  width first; the list then takes the width its splitter left it
  (`LIST_DEFAULT_WIDTH`, 460 pt, up to `LIST_MAX_WIDTH`, 600 pt); the pane
  gets whatever is left, so it is the column that grows when the window
  does. A narrowing window gives that room up in one order: the navigation
  collapses first, then the list is capped at what leaves the pane its
  `CONTENT_MIN_WIDTH` (280 pt), then the pane stops being a column and
  becomes a popup over the list, and the list itself never narrows past its
  `LIST_MIN_WIDTH` (240 pt) — a window narrower than that scrolls it
  sideways instead (`mail_list` switches the scroll area to both axes, and
  the rows keep their minimum width through `virtual_rows`, which lays them
  out at the union of the viewport and the content's own min rect). The
  thresholds are the columns' own widths rather than constants: the
  navigation collapses on crossing below navigation + list + the pane's
  minimum, where the two widths are what the splitters left the panels at
  (egui's `PanelState`, read through `panel_width`). The list's cap is
  placed by writing that same state (`set_panel_width`) rather than only
  clamping it, so widening through the cap follows the window back up; and
  the remembered width to restore to is the app's own (`list_width`),
  because egui on its own only ever remembers what a panel last rendered
  at — the capped width. The list's splitter is off while the cap holds:
  the window is the one making that split. The popup layout
  (`ColumnPlan::popup`: not even the two minimums fit side by side) shows
  the list alone, and a click on a mail opens the pane over it as an `Area`
  — above the list, under the title bar (which `move_to_top` keeps there; a
  new area is otherwise born above it) and under the modals — with a round,
  chevron-only back button floating over the mail's top-left corner
  (`POPUP_BACK_DIAMETER`), shadowed with the window's own shadow — the same
  one the `Load older mail` button floats on — so it reads as on top of
  the mail rather than as a hole in it; it is the one way back to the list,
  and closing the mail is what it does. The navigation is not shown at all
  in that layout. Re-expanding the navigation after any automatic collapse
  stays a manual choice, exactly as it was when the threshold was a
  constant.

- Layout is a three-column mail client: left navigation (collapsible via
  the title-bar toggle; auto-collapses when the window is narrower than the
  three columns need — see the columns bullet above), the message list
  (while a search is active it shows `search_results` instead of the
  selected folder, and `message_card` paints both kinds of row), and the
  reading pane on the right, the flexible column that takes the rest of the
  window, with a "No mail selected" placeholder. Collapsing the navigation
  — by the toggle or by the narrow-window rule — slides the column out
  rather than dropping it: `Panel::show_collapsible` keeps the rows at the
  layout they had at the column's full width and slides the whole column
  off toward the window's edge, clipped, so the reading pane grows into its
  room over the frames of egui's animation time (`Style::animation_time`,
  0.2 s), and the width the splitter gave the column survives the slide — a
  collapsed column reopens as wide as it was. The splitter is not a second
  way to collapse (`drag_to_open(false)`, and the expanded flag
  `show_collapsible` writes back is dropped): the toggle stays the one
  control, the splitter keeps clamping at `min_size` the way it always has,
  and a collapsed column leaves no invisible grab strip — or resize cursor
  — over the message list's left edge.
  Folders indent under their account (`Ui::indent`) without egui's faint guide
  line down the left of the block (`indent_has_left_vline` cleared in
  `nav`): in this column that line reads as a rule through the folder list.
  The chrome — title bar, navigation, and the columns behind the two cards —
  is the scheme's `panel`; the message list's card and the reading pane's
  cards are the lighter `content`. The columns run from the
  window's own top edge, and the title bar floats over them (see the title bar
  section above): every column keeps clear of the bar with the bar's height
  *inside* its scroll area, which scrolls away with what it holds — the
  navigation's accounts and folders, the list's rows, and the reading pane's
  cards, their headers included — which is why all three pass under the
  glass. **The list column is a card
  too** (`list_card`): the panel behind it is chrome, and the card is as tall as
  the list's own content — the bar's height and the rows, and *nothing* below
  the last of them — so a folder with three
  mails leaves the chrome below them instead of a column of empty surface, a
  list that outgrows the window fills the column as it always did, and a column
  with nothing to list (no account, an empty folder, a search with no hits) has
  no card at all, the way the reading pane has none with no mail open. Its height
  is known only once the list has been laid out, so `mail_list` reserves the
  shape and fills it in at the foot of the frame — under the rows, which is
  where it has to be drawn. The card is framed by a hairline down each side,
  drawn for the card's own height rather than the column's, and the reading pane
  is a card of `content` (`CONTENT_CARD_CORNER_RADIUS`) inset by
  `CONTENT_GUTTER` into the chrome; the list's splitter is on its right
  edge, and the pane's card is inset from that edge by the same gutter, which
  is what keeps the mail off the splitter. With nothing selected there is no
  card at all. The first card of what the pane shows starts *below* the
  floating bar — the bar's height is space above it in the scroll area, not
  padding
  inside it — so its top edge and its two rounded corners are on screen; the
  space scrolls away with the card, which is what still lets the mail pass under
  the glass. **A conversation is that card once per mail** — not a card
  holding a card per mail — stacked oldest first with `CONVERSATION_CARD_GAP`
  of chrome showing between two of them, so every mail of a thread sits on the
  pane the way a mail on its own does. **A mail's head is its sender**: the
  list's own avatar in front of the name (`AVATAR_DIAMETER`, with the sender's
  `sender_color` and `sender_initial` — the same circle the rows draw), the
  mail's five actions at that row's right end (`mail_sheet`, the bar's cells in
  a smaller square, `CARD_ACTION_SIZE`, drawn `text_weak` until the pointer
  brings one to the text color — delete, label, forward, reply all, reply: the
  bar's run with flag and export left out, and the label one opens the label
  popup on *this* mail rather than running an action), the chips of the labels
  it carries while it has any (`label_chips`), and the
  subject, the addresses and the date under it. Each card is drawn by the pane and not
  by a frame around its mail: `mail_card` reserves the shape and the pane
  fills it in from the pane's own left edge to its right one, because a card is
  the pane's surface — the mail itself is never tinted, whatever mark it
  carries: a flag is shown in the list and on the bar's cell, not on the mail —
  and a mail wider than the pane scrolls sideways over its
  card instead of dragging the card off the pane with it. The cards are the
  pane's scroll container — headers, rule,
  body and the attachment cards at the foot of each mail scroll together, and
  the scroll bar runs down the pane's edge rather than down a card's — and a
  card still hugs its mail: it is exactly as tall as the mail until the mail
  outgrows the pane. The scroll area scrolls *both* ways: a mail whose layout
  cannot be narrower than the pane (a fixed-width table — most HTML mail) is
  then scrolled over the pane instead of widening the flexible column, and the
  horizontal bar appears only when the mail really is wider than the pane. A
  newly opened mail resets both offsets. The list lays out only
  the rows its viewport shows (`virtual_rows`, which is
  `ScrollArea::show_rows` with this column's arithmetic: the first row starts
  below the title bar's padding, inside the scroll area, so a row range counted
  from the content's own origin would be off by it — and the row a partly
  scrolled viewport starts in could be skipped), and it draws them straight out
  of the account state: mail is never copied to be listed, so a click is
  remembered as the mail it landed on and opened once the list has stopped
  borrowing that state. Both highlights in the list are the mail's whole row: a square
  block spanning the column edge to edge. A mail draws its own edges instead of
  sharing a rule with its neighbour — left, right and bottom in `row_edge`, the
  top in the fainter `row_edge_top`, each a pixel inside its own rect — so what
  separates two mails is this row's top edge against the bottom edge of the row
  above: two pixels, and `item_spacing.y = 0` keeps the layout from adding a
  third. The list's first row draws no top edge — there is no mail above it to
  be separated from, and the edge would land on the title bar's bottom edge,
  which is deliberately bare. Which row is first comes from the caller
  (`message_card`'s `first` flag) rather than from the row's y: the bar's own
  height is padding above the first mail, so it no longer starts at the
  column's top edge. The column's hairlines are `row_edge` too and run its whole height,
  and a row paints them again over its own highlight, which is what keeps a
  selected mail's frame visible across its fill. Pane padding lives *inside*
  each scroll area
  (`Frame::inner_margin(LIST_PADDING)` around the rows — the sides, and the
  bar's height at the top, which is what the rows scroll under; there is no
  bottom padding, because the list's card ends with its last mail — and
  `mail_card` for each mail: the card's own padding all round, and — above the
  first card of what the pane shows, and only that one — the bar's height as
  space in the scroll area rather than padding on the card), not on the panel's
  frame, which is what lets the scroll bars reach
  the column's edge and the pane's edge instead of stopping on the padding.
  egui's panel separators are off in both schemes (they would double those
  hairlines and the card's gutter) and the two boundary pixels are repainted
  with the chrome after the panels, so hovering a splitter draws no line at all.

## Frosted glass

- The frosted backdrop is hand-written GPU code (`src/glass.rs`), because egui
  has no blur: nothing in the painter can sample what has already been drawn.
  On wgpu that is a decision about the *frame*, not something a paint callback
  can do on its own: a texture cannot be sampled by the pass that is drawing
  it, and the pass's target here is the window's surface, which is not even
  created with `COPY_SRC`. So the frame is split around the glass —
  `crate::gpu` renders everything *behind* the bar into an offscreen scene
  texture, `Glass::blur` copies the bar's strip of it into a texture of the
  strip's own size (a texture-to-texture copy, the one thing wgpu allows) and
  blurs that in two passes, and the second half of the frame — the bar's fill,
  its icons, its text — is drawn onto the surface over the blurred strip. Where
  the split falls is egui's own shape list: the glass callback is the first
  shape that belongs to the second half, and the callback is also what draws
  the strip, into the viewport egui has already set to the bar.
  The blur is separable because a single two-dimensional kernel cheap enough to
  run per frame is sampled too sparsely and leaves a comb of aliasing over the
  very letters it is meant to destroy; seventeen taps a pixel and a half apart,
  twice, cost less and dissolve the text instead. One pipeline does all three
  draws, since with a step of zero the blur is a plain copy. Every failure is
  soft: a device that will not take the pipeline or the textures reports once,
  sets `failed`, and leaves the bar translucent over the crisp content. There
  is no multisampling to fight here — this integration asks for none — which is
  what the copy wants anyway.

## The navigation column

- The navigation column is where accounts are switched, which is why the
  title bar has no account menu: every account is a collapsible section
  (`collapsed_accounts` holds the collapsed ids, in memory only), whose header
  shows the name and the account's unread total and whose folders are indented
  under it. An account with no cached folder at all still offers INBOX, since
  clicking it is what starts the first fetch. Selecting a folder of another
  account is what makes it the active one. The whole column is one scroll
  area: the title bar's height is space at its top, so the account sections
  and folders slide under the glass when it is scrolled, and its offset is one
  of the three the bar floats on.
  **Each account row ends with that account's own sync cell**
  (`ACCOUNT_SYNC_SIZE`): the title bar's sync glyph at the row's scale, shown
  under the pointer — its room is reserved whether or not it is drawn, so the
  unread count and the name do not move — with a spinner while that account is
  syncing and a failure dot when its last sync did not finish, both shown even
  at rest because they are status rather than the button. A click is
  `KimoApp::sync_account` — the same `Job::Sync` the title bar's sweep runs, for
  this account alone, including the open folder when the account is the one in
  use and that folder has never been fetched. The cell is registered over the
  row, so a click on it does not fold the section; it is a target on every row,
  the way the bar's cells always are. Its tooltip names the account and says
  when its last sync failed.
  What it draws is a cache of the display model, the way the reading pane
  caches the parsed body: `AccountUiState::nav` derives the rows (`FolderRows`)
  from the account's folder list and mail only when `folders_rev` moved, so a
  frame costs the drawing rather than a decode, a `folders::kind_of` and an
  unread count per folder. The revision moves when a `LIST` named **different**
  mailboxes (`Event::Folders` compares before it replaces), when mail arrives or
  leaves (`Event::Messages`, `Event::MessageDeleted`) and when a mail is read
  (`open_message` marks it seen locally) — so a sync that found the same folders
  leaves the column exactly as it was, and a change is a rebuild of the rows
  rather than a redraw that happens to look the same. Rows come in the two
  groups the folder list defines: first the mailboxes the server lists — in
  `imap::order_mailboxes` order, which is the order a `LIST` answer is given in
  and the order the cached list is drawn in, so a sync does not reorder the
  column — and then the ones it does not, which are here for the mail cached
  under them, drawn weak whatever their unread count with a tooltip saying why,
  and still clickable. A folder of that second kind is only dropped from the
  column when it holds no mail at all; its row in the database stays.
  A Gmail account draws one shape more: its `LIST` names the system mailboxes
  under a `[Gmail]` container that is `\Noselect`, and `SELECT` refuses it, so
  that name is never a row. The folders under it show without the prefix
  (`[Gmail]/All Mail` is *All Mail*), and every other listed mailbox is a
  Gmail label — a synthetic folder, with no mailbox of its own in Gmail's
  store — drawn with the label tag rather than a folder icon. Those rows come
  in `folders::order_gmail`'s order: INBOX, the `[Gmail]` system mailboxes,
  then the labels, each group in name order. A label called `Sent` stays a
  label (the tag, not the Sent icon), while the system mailboxes keep the
  icons their stripped names classify. The wire name is untouched —
  `[Gmail]/All Mail` is still what `SELECT` gets — because the prefix is
  display, not identity.
  **The labels are the column's last group, under every account**, which is
  where a thing that belongs to no account can be: `label_group_header` is
  the word alone — no disclosure triangle, because there is no section to
  collapse, and no count, because the labels are not one list — at the left
  edge the accounts' names start from, and the rows are indented under it the
  way folders are. A row (`label_row`) is a folder row's shape with the
  bar's tag glyph where the icon goes, the name, and the amount of mail carrying
  it right-aligned; the whole row is the click target, and one whose label is in
  the filter takes the selection fill. A label no mail carries any more is drawn
  weak and stays in the column: it is the user's own word rather than a count of
  the mail under it.

- Folders are custom rows — kind icon, name (elided with an ellipsis), unread
  count right-aligned — because the whole row should be the click target and
  carry the selection highlight, which `selectable_label` cannot do once an
  icon sits in front of the text. The icon comes from `folders::kind_of`,
  which classifies the *decoded* mailbox name: an exact keyword first (`INBOX`, `Sent`, `Trash`, `归档`), then
  keyword plus a standard ending (`已删除邮件` = 已删除 + 邮件,
  `Sent Items` = Sent + Items), walking path segments from the right so
  `[Gmail]/All Mail` and `项目/已发送/2026` match as well. Only those
  endings, so `Sentinel` and `已发送的报表` stay ordinary folders. The
  Chinese names are in the table because mailbox names arrive in the server's
  language — that is data, not UI copy, and not an i18n framework (rule 6).
  SPECIAL-USE attributes are still not read (the client never asks for the
  capability), so the name is the only source of a kind. Row icons live in
  `assets/folder-icons/`: white on transparency, 48 px, baked from the SVGs
  in `assets/icons/` with `sed 's/currentColor/#ffffff/g'` plus
  `rsvg-convert -w 48 -h 48`, and tinted per row. White matters because egui
  tints by multiplying (a black bake could never take the text color), and
  `folders.rs`'s test pins the size and the white/transparent pixels.
