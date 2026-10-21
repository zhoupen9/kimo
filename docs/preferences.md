# Preferences — the modal and the automatic sync

Read when touching `preferences_modal`, a key in `settings.json`, or the sync clock.

## The preferences modal

- `preferences_modal()` is the one place to configure Kimo: an
  `egui::Modal` (backdrop click or Escape closes it) with a sidebar of
  pages — **General** (the mail list's threading, the automatic sync, the
  notifications and the window's minimize and close behaviour), **Accounts**
  (account list with
  use/edit/remove, and the add/edit account form in place of the list: it is
  pre-filled from the account it was opened on, and `Save & sync` updates that
  account and syncs it), **Appearance** (theme, accent, Simple/Blitz HTML
  renderer, the interface/fixed-width fonts — all persisted in
  `settings.json` — plus a preview of the palette the theme and the accent
  resolve to) and **About** (the app's icon, the build's version and a short
  summary of what Kimo is). The heading row ends in the cross that closes the modal
  (`PREFS_CLOSE_SIZE`, drawn by `paint_close_icon`, with the scheme's hover
  fill under the pointer) — a modal's cross, not the title bar's danger red:
  closing preferences destroys nothing. Configuration belongs on a page there,
  not as another title-bar toggle; the title bar keeps only actions (syncing,
  window controls), and switching accounts is a navigation-column affair.

- **About reads the build rather than remembering a string**: the version is
  `CARGO_PKG_VERSION`, so a release that bumps `Cargo.toml` cannot leave the
  page claiming another one; the icon is the embedded `assets/app.png` the
  tray serves and notifications write (`crate::tray::APP_PNG`), decoded and
  uploaded the first time the page is drawn rather than at startup. The page
  is deliberately read-only — it is where a reader checks what is running, not
  another place to change it.

## Minimize and close to the tray

- **Minimize stays the window manager's unless asked otherwise**: the General
  page's `minimize_to_tray` (off when absent) makes the title bar's minimize
  cell send `Visible(false)` instead of `Minimized(true)`. It only does so
  while `tray::available()` is true — the tray is the way back, and a hidden
  window with no tray would be out of reach; without one the cell keeps the
  normal minimize. The setting is persisted in `settings.json` like the
  others.

- **Close follows the same rule**: the General page's `close_to_tray` (off
  when absent) makes the title bar's cross send `Visible(false)` instead of
  `Close` (`close_command`). The compositor's own close arrives as
  `WindowEvent::CloseRequested`, and the loop hides the window there too. The
  tray menu's Quit is then the way out; it sets the loop's quit request instead
  of sending a close, because close-to-tray would turn that close into a hide.
  Without a registered tray the cross and the compositor's close both quit as
  they always did.

- **Wayland has no hide request, so the window is dropped**: winit's
  `set_visible` is a no-op there and niri ignores `set_minimized` outright,
  which is why the minimize cell cannot simply minimize on niri. On Wayland
  either `Visible(false)` (the title bar) or `Kimo::hide_to_tray` (the
  compositor's close) is taken as the hide request: `Kimo::hide_window` drops
  the window and its surface, `KimoApp` and the GPU device/renderer (and every
  texture they hold) stay, and a tray Show runs `open_window` again. Hidden
  work — draining workers, notifications, the auto-sync clock — runs through
  `KimoApp::pump_hidden`, woken through `src/wake.rs` rather than egui's
  repaint callback, which can consider an immediate frame already served when
  there is no window to draw it.

## Automatic sync

- **The automatic sync is a preference, and it is the only switch**: the
  preference `auto_sync` (off unless asked for) sweeps *every* account every
  `auto_sync_minutes` (15 by default, clamped to 1–1440) — `KimoApp::sync_all`,
  the same sweep the title-bar Sync cell runs, so an account that is not
  configured is not asked and a failed one reports itself like any other sync.
  What it adds to the startup sweep and the button is the clock and nothing
  else, and the preferences page's controls are live whether or not it is on.
  `pump_auto_sync` is the
  clock, called once a frame: `auto_sync_wait` is the whole decision (nothing
  while off, otherwise the interval left since the last sweep, never negative),
  and the wait it returns is what the app asks egui to repaint after — no other
  timer runs, and a frame with nothing to draw would otherwise never come. A
  sweep already in flight is left to finish and the clock is set anyway, so a
  busy account is asked again a full interval later instead of every frame; the
  clock is reset by `sync_all` itself, so pressing Sync is not a reason to be
  swept a moment later. Changing the preference restarts the clock, and the
  preference is what `settings.json` keeps — the timer is not persisted and
  starts fresh with the process. **A sweep shows itself in the sync cell**, not
  in a strip of its own: a spinner takes the glyph's place while
  `sync_in_flight` holds, a dot sits in the cell's corner while an account's last
  sync stands failed, and the accounts' own words — "syncing…", the error — are
  in the tooltip. What the bar shows of a sync is therefore in the control that
  starts one, and nothing else in the bar has to move aside for it.
