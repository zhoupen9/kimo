# Kimo — the docs

The reasoning behind the code, one file per area. The rules, the commands and
the module map are in [AGENTS.md](../AGENTS.md); these files are what an agent
(or a reader) opens when it is about to change the area in question, and they
are where new reasoning goes when a change is done.

| Doc | What it holds |
| --- | ------------- |
| [architecture.md](architecture.md) | threads, jobs and events; how a sync stays incremental; merge identity; remote resources |
| [storage.md](storage.md) | `mail.db` and its migrations, `settings.json`, the caches, the search index |
| [protocols.md](protocols.md) | the IMAP/POP3/SMTP clients, TLS and the finite-field DH fallback, mailbox names, error mapping |
| [ui-shell.md](ui-shell.md) | the window and its title bar, the three columns and how they give way, the frosted backdrop, the navigation column, the egui 0.36 integration |
| [ui-mail.md](ui-mail.md) | the message list, rows and threading, select mode, the search box and popup, the reading pane, attachments, labels, flags, filters, sort |
| [ui-style.md](ui-style.md) | the house rules a drawn widget obeys: chevrons over solid triangles, the mail address fields' outline, the Cc/Bcc fold |
| [compose.md](compose.md) | the compose card in both its containers, and the mail `outgoing` writes |
| [theme.md](theme.md) | the light/dark schemes and how a mail's own colors are adapted, font discovery |
| [html-mail.md](html-mail.md) | the in-egui subset renderer, the Blitz raster, and Blitz's build and scale rules |
| [platform.md](platform.md) | the portal file dialog, the tray, notifications, packaging |
| [preferences.md](preferences.md) | the preferences modal and the automatic sync |
| [testing.md](testing.md) | running the app, the headless UI dumps, the render and frame benchmarks |
| [limitations.md](limitations.md) | the states that are accepted on purpose — read before "fixing" them |
| [fix.md](fix.md) | the memory plan: what a running instance's resident set is made of, and the order to fix it in |

## Keeping these docs

- A reference to "rule 1", "rule 2", … is a numbered **Hard rule** in
  [AGENTS.md](../AGENTS.md) — those stay in the core file, so a doc can cite
  them by number.
- One file per area of the code, not one per feature: a new bullet belongs
  beside the feature it is about, not appended to the end of the file.
- Every bullet says what was decided, why, and the trap it cost. A bullet that
  only restates what the code says is noise — point at the function instead.
- The rustdoc in `src/` is the first place for a local invariant (what a
  constant is for, why a function takes two widths); these files are for the
  reasoning that spans a module or a decision.
- `AGENTS.md` does not grow: if something is a rule for an agent it goes
  there, and if it is reasoning it goes here.
