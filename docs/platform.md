# Platform — file dialog, tray, notifications, packaging

Read when touching `src/portal.rs`, `src/dbus.rs`, `src/tray.rs`, `src/notify.rs`, or `packaging/`.

## The desktop's own file dialog

- **The file dialog is the desktop's, not Kimo's.** There is no file-picker
  crate: the dialog is asked for over D-Bus from the compositor's own portal
  (`src/portal.rs`, `org.freedesktop.portal.FileChooser`), which keeps the
  dependency list as it is — no GTK, no portal crate with an async runtime behind
  it — and is the only thing that works under Wayland, where winit 0.30 delivers
  no drag-and-drop events at all. Two calls are made: `OpenFile` for the files an
  attachment is chosen with, and `SaveFile` for where an export goes — the save
  told `current_name` and `current_folder`, the name and the folder an export
  would have used itself, so that pressing Save without touching anything gets the
  file a session with no dialog gets. A chooser is a `Job` because the call does
  not return until the user has answered, and the answer arrives as a `Response`
  signal on the request object the reply named (read in whichever order the two
  come: a portal that refuses at once sends the signal first). Two things about
  the wire that cost a debugging session and are worth keeping: **the connection
  greets the bus (`Hello`) before anything else**, because a bus answers anything
  sent before the greeting with an error and then *closes the connection* — which
  arrives as "bus closed the connection" and looks nothing like a missing
  greeting; and **the methods have been `OpenFile` and `SaveFile` for as long
  as the interface has had options** — every release back to 0.10 (2018) declares
  those two and not the bare `Open`/`Save` one might expect, so a refusal means
  "this desktop has no dialog" and is not retried under another name. The token
  that becomes the last element of the request object's path counts rather than
  naming the process alone, because two dialogs can be up at once — an
  attachment's and an export's — and two requests under one token would be two
  requests for one object path. The developer test
  (`cargo test -- --ignored the_desktop_answers_a_chooser_call`) walks both calls
  against the real portal on a session that has one: it opens each dialog and
  closes the request the moment the reply names it.

## Tray icon

- **Tray icon:** `src/tray.rs` implements StatusNotifierItem by speaking the
  D-Bus wire protocol directly (`src/dbus.rs`) on a `std::thread` —
  deliberately not zbus (async executor, rule 2) or the GTK tray crates (C,
  rule 1). The icon is
  `assets/app.png` (256px, rasterized from the multi-color `assets/app.svg`
  — regenerate with any svg→png tool, e.g.
  `rsvg-convert -w 256 -h 256 -o assets/app.png assets/app.svg`), embedded
  with `include_bytes!` and decoded with the image crate, keeping its own
  colors (the purple circle works on any panel). Keep the source large: the
  host scales the pixmap down to its panel size, so a 48px icon looked
  blurry on a HiDPI panel. The cost is 256 KB of ARGB per `IconPixmap`
  reply, which is nothing on the session bus. Left click raises the window —
  reopening it after a Wayland hide, by minimize-to-tray or close-to-tray —
  and the right-click menu is a minimal `com.canonical.dbusmenu` object at
  `/Menu` (Show / Quit) served by the same thread. Quit sets the loop's quit
  request before it wakes it: a hidden window has no close command to receive,
  and with close-to-tray on a close command would be taken as a hide rather
  than an exit. Missing
  StatusNotifierWatcher disables the tray silently, and `IconPixmap` must
  stay `a(iiay)` — hosts cannot read a bare byte array. The session bus is
  silent for hours at a time, so `dbus`'s reads ride out an idle read-timeout
  expiry instead of failing on it (`read_exact` turned it into a fatal
  `WouldBlock`, and the icon vanished 30s after startup) — one rule for both
  D-Bus threads. The thread publishes `tray::available()` once registration
  succeeds, which is what lets minimize-to-tray and close-to-tray hide the
  window only while there is a tray to bring it back; Show and a left click
  unminimize before they raise.

## Desktop notifications

- **Desktop notifications:** `src/notify.rs` announces new mail through the
  freedesktop `org.freedesktop.Notifications` interface — the same raw-D-Bus
  choice as the tray, made on the shared wire layer in `src/dbus.rs` and for
  the same two reasons (no `zbus` async executor, rule 2; no C, rule 1).
  Showing one is a blocking call, so it runs on one dedicated `std::thread`
  that owns the connection: `notify::show` only posts to it, whether the
  caller is the UI or a sync worker (rule 3). The icon is the brand PNG the
  tray serves and the title bar draws, written out as a `48 · scale` px file
  under the resource cache and handed to the daemon as an `image-path` hint:
  48 logical points, which is a real 96 px bitmap on a 2× screen instead of an
  upscaled one. `image-data` is deliberately *not* used — daemons disagree
  about the byte order of its pixels — and the app icon itself is the `kimo`
  name the desktop file installs, with a `desktop-entry` hint so a shell can
  file the notification under Kimo's own entry.
  What is announced is only news: `Event::Messages` carries `new_mail`, true
  for a fetch that resumed at the folder's mark (IMAP) or found UIDLs the
  database did not hold (POP3), and never for a folder's first fill,
  `Load older mail` or the copy of a mail that was just sent. The app then
  drops what is not worth a popup: mail already read, folders whose mail is
  not *delivered* (Sent, Drafts, Trash, Junk, and Gmail's All Mail and
  Starred, where every mail of the account turns up a second time), and a mail
  the account already holds under another mailbox (the same `Message-ID`).
  Up to `NOTIFY_MAX` (3) arrivals in one batch are one notification each;
  more become a single one counting them, so catching up with forty mails is
  one popup and not forty. A finished **export** raises one of its own
  (`exported_notification`: "Mail exported" / "Saved to <path>"), which is
  where that answer lives rather than above the message list — the click was in
  the title bar and the file it wrote is not in the list. Preferences → General
  has the switch for *new mail* (`notifications` in `settings.json`, absent
  meaning on), and an export is deliberately not gated by it: the switch is
  about mail that arrives by itself, while an export answers a click the user
  has just made, and a click answered with silence is worse than one answered
  where they are not looking. A session with no notification daemon is simply
  quiet. `cargo test notify_probe -- --ignored` under `dbus-run-session` with
  `dbus-monitor` watching is how the wire form is checked by hand.

## Packaging

- **Packaging:** `packaging/linux/kimo.desktop` is the launcher entry for the
  `kimo` app_id, to be installed as
  `~/.local/share/applications/kimo.desktop`. The icon on its own
  (`~/.local/share/icons/hicolor/scalable/apps/kimo.svg`) satisfies
  `niri/window`, which looks the app_id up in the icon theme, but waybar's
  `wlr/taskbar` resolves a `Gio.DesktopAppInfo` instead and shows the grey
  `image-missing` placeholder without a desktop file. `Exec=kimo` has to
  resolve in `$PATH` — GLib silently rejects a desktop file whose `Exec`
  names a program it cannot find — which the file's header comments record.
