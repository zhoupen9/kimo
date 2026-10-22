# Memory fix plan

A plan, not a change: what the resident set of a running Kimo is actually made
of, which parts of it are the app's own decision rather than the platform's, and
in what order to attack them. Three of the items have landed (§ "What has
landed") — the GPU, the allocator and the CJK pick; the rest are not
implemented yet, and each says what to change, why, what it is expected to buy,
and how to tell afterwards that it worked.

Read with [testing.md](testing.md) (how a render change is verified) and
[architecture.md](architecture.md) (the UI/worker line every item below has to
stay on the right side of).

## What has landed

The first two items are in the tree, measured on the same box, same mailbox,
idle two minutes: **idle RSS 253.3 MB → 208.4 MB, `[heap]` 46.5 MB → 17.6 MB**.
The CJK pick is in the tree too and adds no number of its own.
(The table below says 227–233 MB; that is the session the plan was written in.
The pair above is one session, the same database on either side of the change,
which is the comparison a candidate change has to win.) The item numbers are
the ones the plan always had, not a renumbering.

- **Item 1, the GPU is asked once** (`src/gpu.rs`): one backend at a time,
  Vulkan and then GL, with the adapter logged once at start-up. The GL backend
  came up beside the Vulkan device that draws — EGL and GBM initialised, Mesa's
  GL driver resident — for about 21 MB that no frame used. See
  [ui-shell.md](ui-shell.md). A follow-up turns the *driver list* into a choice
  (Preferences → Graphics, `src/gpus.rs`): the other cards' ICDs are never
  opened at all, which is where most of rows 1–2 below come from. Measured on
  the same box with an empty database: every driver 190 MB RSS, the Intel card
  picked 110 MB, with no `libGLX_nvidia`, `libnvidia-*`, `libvulkan_lvp`
  or `libLLVM` in the process. It is **not** a change to the default — with no
  pick, every installed driver still loads, and the numbers in this plan
  describe that path.
- **Item 2, the allocator's threshold is pinned** (`src/trim.rs`, called from
  `main`): `mallopt(M_MMAP_THRESHOLD, 128 KiB)` stops glibc's dynamic
  threshold from rising, so a 19 MB font read, a rasterised page or a fetched
  mail is a mapping of its own, returned when it is dropped. That was 232 →
  208 MB, matching `MALLOC_MMAP_THRESHOLD_=131072` to within a megabyte. The
  four `malloc_trim` calls this plan asked for released nothing measurable
  once the threshold was pinned, and are not in the tree. See
  [architecture.md](architecture.md).
- **Item 3b, the CJK face is a preference** (`cjk_font` in `settings.json`, a
  third picker on the Appearance page, `fonts::missing_cjk_samples`): a family
  name that replaces the automatic choice, an unset path that leaves today's
  behaviour untouched, a warning for what the picked face has no glyph for, and
  no second copy of the default loaded. It saves no memory by itself — it
  decides *which* file item 3 will map. See [preferences.md](preferences.md) and
  [theme.md](theme.md).

What the standing target still wants is item 3 (mapping the CJK face, now that
3b says which one), items 6–7 (the caches) and item 4 (the ceiling the mail
cache must not pass). The driver pages in rows 1–2 are not the app's to free
once they are in, but the loader opening an unused card's ICD is: item 1's
follow-up keeps them out of the process when a card is picked, and that is
where the ~80 MB measured above goes.

## The measurement this plan comes from

One idle instance, 2 minutes after start, 161 cached mails in 24 folders across
2 accounts, NVIDIA proprietary driver + Mesa + lavapipe installed, Wayland on
niri, `html_renderer: "egui"`:

| Metric | Value |
| --- | --- |
| RSS | 227–233 MB (VmHWM 230 MB — the peak is the start-up cost) |
| PSS | 151 MB |
| RSS anon (app + allocator) | 117 MB |
| RSS file-backed (drivers, libs, own binary) | 113 MB |

RSS was flat over 3 minutes of idling: **nothing here is a leak**, every item is
either a start-up cost or a retention.

Breakdown, from `/proc/<pid>/smaps`, grouping rows by backing file:

| # | Region | RSS | Whose |
| --- | --- | --- | --- |
| 1 | NVIDIA driver stack (`gpucomp` 27.1, `rtcore` 10.4, `glcore` 9.5, `eglcore` 6.7, `glvkspirv` 2.9, GLX/EGL 1.9) | 59.6 MB | platform, inflated by us |
| 2 | Mesa / LLVM / Vulkan ICDs (`libLLVM` 15.7, `gallium` 8.2, `vulkan_intel` 4.6, `vulkan_lvp` 2.5, EGL/gbm) | 32.5 MB | platform, inflated by us |
| 3 | Kimo's own text + rodata (egui, stylo/Blitz, image codecs, rustls, ureq) | 20.3 MB | us |
| 4 | `[heap]` — main malloc arena | 56.7 MB | us, ~half of it free |
| 5 | anon arena holding one 19 MB font | 19.0 MB | us |
| 6 | anon arena holding D-Bus/SNI marshalling leftovers | 24.9 MB | us, ~80 % of it free |
| 7 | other (libc, wayland, xkb, thread stacks) | ~11 MB | platform |

The headline: **92 MB of driver stack (rows 1–2) and ~50 MB of allocator
retention (rows 4–6) for an app whose own mail data is 4.6 MB.** The mail
content of the whole database — 3.87 MB of `body_html`, 0.21 MB of
`body_text`, 0.49 MB of inline images — does not appear in the table at all.

Reproduce (any build, any machine):

```
grep -E 'VmRSS|RssAnon|VmHWM' /proc/$(pgrep -x kimo)/status
awk '/^[0-9a-f]+-[0-9a-f]+ /{n=$6} /^Rss:/{s[n]+=$2}
     END{for(k in s) printf "%8.1f MB %s\n", s[k]/1024, k}' \
    /proc/$(pgrep -x kimo)/smaps | sort -rn | head -30
```

A caveat worth keeping: rows 1–2 are *this* box (NVIDIA + Mesa + a software
Vulkan ICD). On a machine with one GPU driver and no lavapipe they would be a
third of that. The plan therefore treats them as "the largest avoidable part",
not as "Kimo is a 230 MB app".

## Non-goals

- Not replacing egui/wgpu, not dropping Blitz, not touching the two-renderer
  design — those are the project's shape (see [html-mail.md](html-mail.md)).
- No async runtime, no new C dependency, nothing that blocks `KimoApp::ui`
  ([AGENTS.md](../AGENTS.md) rules 1–3). Items 5 and 6 below are the ones that
  could tempt someone over that line; they are written to avoid it.
- Not chasing the 20 MB of code pages (item 8) until the cheap items are done.

---

## 3. Keep the CJK fallback out of the private heap (19 MB, row 5)

**Evidence.** The 19.0 MB anon region contains `cmap`, `GSUB`, `NotoSans` and
the face's glyph-name table: it is `NotoSansCJK-Regular.ttc` (19 MB on disk)
read whole by `fonts::cjk_font` (`fonts.rs:588`) and handed to
`egui::FontData::from_owned` by `fonts::install` (`fonts.rs:451`). It is
private dirty memory for the life of the process, and `install` runs at least
twice at start-up (`app.rs:2049` with no picked families, then again from
`Event::FontsScanned` at `app.rs:3472`), so the file is read twice and the
first copy is dropped — a 19 MB transient on top of the 19 MB permanent cost.

**Change.**

1. Back the CJK face with a **read-only mapping** of the file instead of a
   `read` into a `Vec`: map it, hand egui a `'static` slice over it (the
   mapping is process-lifetime, so leaking the handle is honest), keep the same
   candidate list and the same `index` selection by script. Then those pages
   are file-backed and evictable under pressure, and shared with anyone else
   using the same font.
2. Do not load it twice: `install` should not re-read a face it already holds
   when only the picked families changed — the CJK font key is a constant
   (`CJK_KEY`), so the second call should reuse what the first one installed.
3. Do not answer this with a silent "small file unless the locale is CJK" rule:
   the pick (item 3b, landed) is that decision, made by the reader, and a
   heuristic would change the glyphs they look at without being asked.

**Expected saving:** 19 MB of private dirty memory becomes file-backed and
reclaimable (PSS drops by most of it; RSS may still count touched pages, and
should be reported both ways), plus a one-off ~19 MB transient at start-up.

**Risk / trap.** `FontData` needs an owned or `'static` buffer, so the lifetime
work is real: a `Mmap` must outlive the font set and must not be unmapped while
egui's atlas still refers to the faces. Getting this wrong is a use-after-free
that shows up as garbage glyphs days later, not as a crash on the next frame.
If that feels too sharp, keep `read` and take only (2) — the transient is the
cheap half.

## 4. The message list should not need the mail (rows 4, and the 1.4 GB cliff)

**Evidence.** `db::load_mailboxes` (`db.rs:1045`) selects every row of
`messages` for every folder of every account with `body_text`, `body_html` and
the `inline_images` blobs inline (`db.rs:1101`), at start-up and after every
sync that reloads. The list row needs subject, from, date, preview and flags;
the pane needs the body — and only for the one mail (or conversation) that is
open.

Today that costs ~28 KB per mail (4.6 MB of 161) and looks harmless. It is
linear in the size of the cache, not in the size of the window: a 50 k-mail
archive would be ~1.4 GB of resident strings, and the user would experience it
as "Kimo takes a minute to start and the desktop swaps".

**Change.** Split the load:

- the folder/list path returns metadata only (no body columns, no inline
  blobs), which is also less SQLite page traffic;
- a new body fetch loads one mail's `body_text`/`body_html` and its inline
  images by `(account, folder, uid)`, on the pattern `BodyCache` already keys
  by (`app.rs:1597`);
- the reading pane asks for the body when a mail becomes visible and shows the
  existing loading state while it waits;
- `MailMessage` grows a cheap "body not loaded" state so nothing can
  accidentally render an empty body as a real one — that is the failure mode to
  design against, not the memory.

**Where this must run.** The body fetch is SQLite I/O, so it is a `Job` on a
worker thread, like the sync jobs; the UI thread only posts the request and
draws the placeholder ([architecture.md](architecture.md), rule 3). The answer
comes back as an `Event`, and the merge must tolerate the mail having been
re-fetched, moved or deleted in the meantime.

**Expected saving:** ~3–6 MB on this database today; the point of the item is
the ceiling, not the saving. This is the only item in the plan that changes
behaviour a user can notice (a short "loading" state where mail appears
instantly now), and the only one that touches `sync`, `search`'s previews and
the offline story — schedule it as its own change, not as a memory cleanup.

**Risk / trap.** Mail that the server no longer has still has to render from the
cache — the body is in the database, so this stays true; but any code path that
assumed "the `MailMessage` I hold has the body" now has one more state. The
search results list is built from the FTS index, not from these strings, so it
must keep working without the bodies (`search.rs`, `load_mailboxes`' siblings)
— verify a search hit opens its mail.

## 5. Stop cloning conversations (row 4, duplication)

**Evidence.** When a conversation is opened, its mails are cloned field by
field, bodies included (`app.rs:3582`: `thread.messages…map(|`app.rs:3579`, `messages: thread.messages…clone()`i| messages[i].clone())`), on
top of the copies already held by the folder. The reading pane then shows the
clone while `BodyCache` keys the parsed document by `folder/uid` — two owners of
the same strings for one mail.

**Change.** Make the open conversation refer to the folder's mails instead of
copying them: keep the existing `Vec<usize>` indices (`Thread::messages` already
is one) or an `Arc<MailMessage>` for the cases where the list it points into can
be replaced by a sync while the pane is open. The cache invalidation rule is the
same one `BodyCache`'s `fingerprint` already implements (`app.rs`: a re-fetch
that changes a body must be noticed) — reuse it rather than inventing a second
one.

**Expected saving:** the size of the open conversation's bodies (~1–2 MB now,
bounded by the largest thread). Worth doing while item 4 is in progress, since
both change who owns a body; alone it is not worth the churn.

## 6. Bound the parsed bodies by bytes, not by count

**Evidence.** `BODY_CACHE` (`app.rs:1627`) keeps 8 parsed html5ever documents.
A DOM costs several times its source, and this database's mail includes 371 KB
Confluence-table bodies (thousands of `<td>`/`<span>`): the heap dump of a
running instance is dominated by those fragments. Eight of those is 20–40 MB;
eight of a plain mail is 200 KB. A count cannot express "keep the recent ones
while they are cheap".

**Change.** Keep the entry-count cap as a ceiling, add a byte budget (the sum of
the source `body_html`/`body_text` lengths, which is already known at
fingerprint time) and evict least-recently-used until under it. Same
`used`/`touch` bookkeeping, no new data structure.

**Expected saving:** 5–20 MB on a mailbox full of HTML-heavy mail; nothing on a
light one. Cheap and self-contained.

## 7. Same question for the rasters and remote images

**Evidence, not yet measured.** `BLITZ_PAGES_CACHED` is 2 (`app.rs:10071`) and
each page is "a few megabytes of texture"; `images`, `inline_textures` and
`stylesheets` (`app.rs:812`–`819`) are per-account maps with no size cap. With
`html_renderer: "egui"` today these cost little, so this item exists to make
sure the memory story stays honest when the Blitz renderer is the one in use —
and to stop an image-heavy thread from becoming the peak.

**Change.** Give the per-account image and inline-texture maps the same
byte-budget eviction as item 6 (a texture's size is known when it is decoded).
Leave `BLITZ_PAGES_CACHED` alone until there is a measurement; a page tile count
is a rendering-quality decision, not a memory one.

## 8. Optional: make Blitz a build feature

**Evidence.** Weigh 20.3 MB of resident code and 35 MB of binary against a
renderer that the current settings do not use (`"html_renderer": "egui"`); the
Blitz tree is stylo, taffy and vello, the largest single thing in the binary.

**Change.** A Cargo feature (default on, so nothing changes for the packaged
build) that compiles `blitz_render` out, and the `html_renderer` preference
refusing "blitz" when it is not compiled in. Only worth doing if a "simple"
build is something the project wants to ship.

**Risk / trap.** This is not a memory fix for the default build — it is a second
build to test. Two renderers behind one flag is how the last C-library
exception nearly became a precedent (see rule 1's history); decide it
explicitly or not at all.

---

## Order of work

| Order | Item | Saving (this box) | Risk | Touches |
| --- | --- | --- | --- | --- |
| 1 | Item 3 (2), then (1) | 19 MB + transient | medium (lifetime) | `fonts.rs` |
| 2 | Item 6, then 7 | 5–20 MB | low | `app.rs` |
| 3 | Item 4 | 3–6 MB now, unbounded later | high (behaviour) | `db.rs`, `app.rs`, `sync.rs` |
| 4 | Item 5 | 1–2 MB | medium | `app.rs` |
| 5 | Item 8 | 20 MB binary | medium | `Cargo.toml`, packaging |

Ordering rationale: the baseline every number below is compared against is now
208 MB, from 253 MB when the plan was written, because items 1 and 2 landed
first — they were independent of everything a reader sees. Item 3 is next for
the same reason: the CJK face becomes a mapping instead of a copy, and nothing
on screen changes. Items 6 and 7 are cache budgets in `app.rs`, self-contained
and cheap.
Items 4 and 5 change who owns a body, and 4 in particular changes the path mail
takes from the database to the pane, so it goes alone, after the cheap wins,
when the numbers are already good enough that nobody is tempted to do it
carelessly.

**Targets to hold the work to** (same box, same mailbox, idle 2 minutes):

- RSS ≤ 150 MB, PSS ≤ 105 MB — down ~35 % from 227/151;
- no `libLLVM` / `libgallium` / `vulkan_lvp` rows in `smaps` — with a card
  picked in Preferences → Graphics; without a pick the loader still opens every
  installed ICD by design;
- resident memory that does not grow with the size of the mail cache (item 4) —
  demonstrated, not asserted: the 161-mail database and a 10× copy of it must
  idle within a few MB of each other.

The landed items moved the same box from 253 MB to 208 MB RSS (184 MB PSS,
measured in the same session as the 208). The targets above were set from the
227/151 MB the plan was written with; compare PSS only within one session, since
it counts a shared page once.

## How each item is proven

- **Numbers:** the `smaps` recipe above, before and after, plus `VmHWM` for the
  start-up transient. Report RSS and PSS: the driver rows are shared, so RSS
  flatters a saving that PSS shows honestly.
- **Behaviour:** `cargo run` and click through the affected area — CJK mail for
  item 3, opening a thread and a folder switch for items 4–5, an HTML-heavy mail
  for 6–7. The layout has no golden tests ([testing.md](testing.md)); a rendered
  dump is not the app.
- **The default path stays bit-identical:** with `cjk_font` unset, item 3b must
  change nothing a reader can see — same face chosen by locale, same coverage.
  Compare a CJK and a Korean mail before and after on a box without a pick.
- **Gates:** `cargo build`, `cargo test`, `cargo clippy --all-targets -D
  warnings`, `cargo fmt` — before calling any item done.
- **Docs:** whatever each item teaches (the trap, the rejected alternative, the
  number) goes beside the feature it belongs to: item 3 in [theme.md](theme.md),
  item 3b in [preferences.md](preferences.md) (the key and the picker) and
  [theme.md](theme.md) (the coverage table and why the pick is a trade), items
  4–7 in [ui-mail.md](ui-mail.md)/[storage.md](storage.md). This file stays a
  plan: it is deleted or emptied once the items have landed where they belong,
  and any state the project decides to keep as-is is recorded in
  [limitations.md](limitations.md) instead.
