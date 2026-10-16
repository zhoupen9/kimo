# Preferences — the modal and the automatic sync

Read when touching `preferences_modal`, a key in `settings.json`, or the sync clock.

## The preferences modal

- `preferences_modal()` is the one place to configure Kimo: an
  `egui::Modal` (backdrop click or Escape closes it) with a sidebar of
  pages — **General** (the mail list's threading, the automatic sync and the
  notifications), **Accounts**
  (account list with
  use/edit/remove, and the add/edit account form in place of the list: it is
  pre-filled from the account it was opened on, and `Save & sync` updates that
  account and syncs it) and **Appearance** (theme, accent, Simple/Blitz HTML
  renderer, the interface/fixed-width fonts — all persisted in
  `settings.json` — plus a preview of the palette the theme and the accent
  resolve to). Configuration belongs on a page there, not as another
  title-bar toggle; the title bar keeps only actions (syncing, window
  controls), and switching accounts is a navigation-column affair.

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
