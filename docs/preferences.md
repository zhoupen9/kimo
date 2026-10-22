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
  renderer, the interface, fixed-width and CJK-fallback fonts — all persisted
  in `settings.json` — plus a preview of the palette the theme and the accent
  resolve to), **Graphics** (the card Kimo draws with — and with it which
  Vulkan drivers are loaded at all; see below) and **About** (the app's icon,
  the build's version and a short summary of what Kimo is). The heading row ends in the cross that closes the modal
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

## The CJK fallback

- The Appearance page's `cjk_font` names the family egui appends after the
  picked Interface and Fixed-width fonts. It is a *coverage and forms*
  preference, not a memory fix: it changes which file is read, and its saving
  is bounded by that file's size. **Unset is the automatic choice** — the first
  of `fonts::CJK_CANDIDATES` that is installed, with the face the locale asks
  for (`CjkScript`) — so a distribution without `noto-fonts-cjk` still renders.
  A pick overrides the choice, never the mechanism.
- The picker lists every installed family, like the other two, because the scan
  already returns one `FontFamily` per family *with its `path` and `index`*:
  "Noto Sans CJK JP" and "Noto Sans CJK SC" are separate entries, and the
  picked family *is* the face. That is the question `LANG` cannot answer — a
  reader whose session is `en_US.UTF-8` but whose mail is Japanese gets
  Simplified forms otherwise, and today the only workaround is exporting a
  locale they do not want for the whole desktop.
- **Warn, do not filter.** The scanner reads only a face's `name`, `OS/2` and
  `post` tables, so it cannot know whether a family has Han, kana, Hangul or
  Bopomofo without reading every `cmap` — the cost the module exists to avoid.
  The picked file is probed once instead, when the pick is applied
  (`fonts::missing_cjk_samples`: 中 あ 한 ㄅ), and what it lacks is said under
  the picker. The warning **names the scripts** — Han, kana, Hangul,
  Bopomofo — rather than drawing the samples: a face that lacks them draws
  boxes, so a message spelling them out would be four boxes in the very font it
  warns about. Nothing is substituted silently: a reader who picks a Latin
  family has to see why their mail turned into boxes.
- A name that no longer resolves — a `settings.json` carried from another
  machine — falls through to the automatic face and says so. The key is left in
  place, so a machine that has the family uses it again.
- When a pick is set the default collection is **not** loaded as well: a pick
  that cost more memory than the default would make the preference pointless.
- **Two renderers, one pick.** The choice applies to the egui renderer — the app
  UI and Simple HTML mail, in both Proportional and Monospace — and Blitz keeps
  its own curated font set matched by family name. That is the same split as
  the rest of the font story (`docs/theme.md`), and following the pick into
  Blitz would be a separate change with its own verification.

## The graphics card

- **The pick is a Vulkan-driver decision, not an adapter preference.** A
  `wgpu::RequestAdapterOptions` can only choose among devices whose drivers
  have already been loaded, and the Vulkan loader opens every installed ICD as
  it creates the instance. Preferences → Graphics therefore names the card,
  and `crate::gpus` hands the loader that card's manifests alone
  (`VK_DRIVER_FILES`, plus `VK_ICD_FILENAMES` for a loader older than
  1.3.207 — Ubuntu 22.04 ships 1.3.204) before `main` builds the event loop —
  the last moment at which the list can still be chosen. The key is `gpu`, the card's
  PCI address (`"0000:03:00.0"`); absent means the loader's own choice, which
  is what Kimo did before this page existed.

- **The page lists the machine's cards, not a saved list.** `Job::ScanGpus`
  reads sysfs (`/sys/class/drm/cardN/device/uevent`: the PCI class, the ids
  and the slot), the ICD directories the loader reads, and the few `pci.ids`
  lines that name those cards; the radios, the driver line and the "what will
  load" sentence all come from that one scan. It runs the first time the page
  is drawn, on a worker thread — sysfs, a 1.7 MB database and several
  directories are filesystem work the UI thread must not do — and the page
  shows a spinner until the answer arrives.

- **Every failure falls back to every driver, and the page says which value it
  ignored**: a slot that is not installed (an eGPU on another desk, a
  `settings.json` from another machine), a value that is no PCI address, and a
  card whose vendor no installed ICD serves. A manifest is dropped only when
  its library's name attributes it to another vendor (`libvulkan_nvidia.so`)
  or to a software rasterizer (`libvulkan_lvp.so`); a name the table does not
  know is kept. A `gpu` of the wrong JSON type is read as absent
  (`storage::lenient_gpu`) instead of failing the whole file, because one key
  must not cost every other preference. A session that sets
  `VK_DRIVER_FILES` itself is left alone, and the log says so.

- **The choice is the next start's.** A loaded driver cannot be unloaded, so
  the page says a change applies when Kimo starts again; it writes `gpu` on
  the click and the page draws the value as written. `crate::gpu` asks the
  same plan for its adapter, matching the card's PCI bus id from
  `VK_EXT_pci_bus_info` — so a machine whose two cards share one vendor's
  driver still renders on the card that was picked.

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
