//! The Blitz reading-pane page: render identity and cancellation, tile cache,
//! page reuse and eviction, paper choice, and the painted page with its links.

use super::*;

/// Blitz reading pane: page margin, width quantization for renders, and how
/// many rendered pages stay in memory (each is megabytes of texture).
pub(super) const BLITZ_PAGE_MARGIN: f32 = 8.0;
pub(super) const BLITZ_WIDTH_STEP: u32 = 32;
pub(super) const BLITZ_PAGES_CACHED: usize = 2;
/// The paper a Blitz page is drawn on when the mail declares no background of its
/// own and the reader has switched this mail away from white: the dark scheme's
/// content surface, so a dark paper is the surface the app draws mail on
/// elsewhere rather than a colour of its own.
pub(super) const BLITZ_PAPER_DARK: egui::Color32 = crate::theme::DARK.content;
/// How long the pane geometry must hold still before the reading pane lays the
/// mail out for it again. Dragging the splitter, or a compositor animating a
/// window resize, changes it every frame; both renderers wait the same time,
/// for the same reason — every intermediate width would be work that is
/// discarded as soon as the pointer moves again. What they do while they wait
/// differs: Blitz renders on a worker and has nothing to show until a render
/// lands, while the built-in renderer keeps drawing the width it has (see
/// `BodyCache`) and so reflows only once, when the drag is over.
pub(super) const GEOMETRY_SETTLE: Duration = Duration::from_millis(180);
/// A finished page may be scaled down instead of re-rendered while the pane
/// stays within this factor of the width the page was laid out for. Re-flowing
/// the text for a few percent is not worth a render — and it keeps the
/// vertical scrollbar, which narrows the pane when it appears, from
/// invalidating the very page that made it appear.
pub(super) const BLITZ_REUSE_RATIO: f32 = 1.25;
/// Debounce for the reading pane's geometry. `pending` is the key seen last
/// time and when it was first seen, `now` the current instant.
pub(super) fn blitz_settle(pending: Option<(&str, Instant)>, key: &str, now: Instant) -> BlitzWait {
    match pending {
        Some((pending, since)) if pending == key => {
            let waited = now.saturating_duration_since(since);
            if waited >= GEOMETRY_SETTLE {
                BlitzWait::Start
            } else {
                BlitzWait::Wait(GEOMETRY_SETTLE - waited)
            }
        }
        // Another geometry was pending, or none: this one starts settling now.
        _ => BlitzWait::Wait(GEOMETRY_SETTLE),
    }
}
/// Cache key of a rendered page: the mail's identity plus the width, in
/// physical pixels, it was rendered for. Deliberately without the pane height
/// — see `crate::blitz_render::PageRequest::viewport_height`.
pub(super) fn blitz_key(identity: &str, width: u32, scale: f32) -> String {
    format!("{identity}{width}@{scale:.2}")
}
/// Whether a page rasterized `rendered` device pixels wide can be scaled down
/// for a pane wanting `wanted` instead of being rendered again. Only downwards:
/// a raster is never scaled up, that would blur the text.
pub(super) fn blitz_page_reusable(rendered: f32, wanted: f32) -> bool {
    rendered >= wanted && rendered <= wanted * BLITZ_REUSE_RATIO
}
/// Round `value` up to the next multiple of `step`, so a pane resize only
/// re-renders in coarse steps.
pub(super) fn quantize(value: f32, step: u32) -> u32 {
    let value = if value.is_finite() {
        value.max(1.0)
    } else {
        1.0
    };
    let step = step.max(1) as f32;
    ((value / step).ceil() * step) as u32
}
/// Blit a finished Blitz page and re-attach its links as clickable boxes.
///
/// The page is drawn on `paper`: a mail that declares an html or body
/// background has it painted across the whole page by the renderer's own canvas
/// fill (`blitz-paint`'s `paint_scene`), which covers what is drawn here, so
/// this shows only where the mail painted nothing at all — which, for a mail
/// written for white paper, is most of it.
pub(super) fn draw_blitz_page(ui: &mut egui::Ui, page: &RasterizedPage, paper: egui::Color32) {
    if page.truncated {
        ui.label(
            egui::RichText::new("This message is too tall to render in full — the end is missing.")
                .small()
                .weak(),
        );
    }
    let row_width = ui.available_width();
    // The page can be wider than the pane: fixed-width mail is laid out at its
    // own width and scaled down here, so nothing is cut off.
    let fit = ((row_width - BLITZ_PAGE_MARGIN * 2.0) / page.size.x).min(1.0);
    let (_, space) = ui.allocate_space(egui::vec2(
        row_width,
        page.size.y * fit + BLITZ_PAGE_MARGIN * 2.0,
    ));
    let origin = egui::pos2(
        space.left() + BLITZ_PAGE_MARGIN,
        space.top() + BLITZ_PAGE_MARGIN,
    );
    // Under the tiles, because a painter draws in the order it is given: the
    // paper is the page box, scaled with the page like the tiles are.
    ui.painter().rect_filled(
        egui::Rect::from_min_size(origin, page.size * fit),
        0.0,
        paper,
    );
    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    for tile in &page.tiles {
        let rect = egui::Rect::from_min_size(origin + tile.offset * fit, tile.size * fit);
        // White is egui's "no tint" (it multiplies), not a scheme color: the
        // page is shown exactly as Blitz rasterized it.
        ui.painter()
            .image(tile.texture.id(), rect, uv, egui::Color32::WHITE);
    }

    for (rect, href) in &page.links {
        let rect = egui::Rect::from_min_size(origin + rect.min.to_vec2() * fit, rect.size() * fit);
        let id = egui::Id::new((
            "blitz-link",
            href,
            (rect.min.x * 10.0) as i32,
            (rect.min.y * 10.0) as i32,
        ));
        let response = ui.interact(rect, id, egui::Sense::click());
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            ui.painter().rect_filled(
                rect,
                2.0,
                ui.visuals().selection.bg_fill.gamma_multiply(0.35),
            );
        }
        if response.clicked() {
            crate::html::open_url(ui, href);
        }
        let response = response.on_hover_text(href.as_str());
        // A Blitz page is a raster: its links are the only parts with an
        // address, so the menu a link offers is the page's context menu.
        egui::Popup::context_menu(&response)
            .width(CONTEXT_MENU_WIDTH)
            .show(|ui| {
                ui.set_min_width(CONTEXT_MENU_WIDTH);
                ui.spacing_mut().item_spacing.y = 1.0;
                if context_menu_item(ui, "Open link", true, false, false).clicked() {
                    crate::html::open_url(ui, href);
                }
                if context_menu_item(ui, "Copy link address", true, false, false).clicked() {
                    ui.ctx().copy_text(href.clone());
                }
            });
    }
}

impl KimoApp {
    /// Drop every cached Blitz page belonging to an account (keys start
    /// with its id and a `/`, so `work` cannot match `work-2`) — and the papers
    /// its mails were switched to, which are no mail's any more.
    pub(super) fn forget_blitz_pages(&mut self, account_id: &str) {
        self.forget_blitz_prefix(&format!("{account_id}/"));
        self.blitz_paper.retain(|key, _| key.account != account_id);
    }
    /// Drop the cached pages of one message. The key a page is stored under is
    /// the message's identity — account, folder and uid, each followed by a
    /// `/` (`blitz_identity`) — with the geometry after it, so the identity is
    /// the prefix.
    pub(super) fn forget_blitz_message(&mut self, account_id: &str, folder: &str, uid: &str) {
        self.forget_blitz_prefix(&format!("{account_id}/{folder}/{uid}/"));
    }
    /// Drop every cached page whose key starts with `prefix`, and give up the
    /// one waiting for the geometry to settle if it is one of them.
    pub(super) fn forget_blitz_prefix(&mut self, prefix: &str) {
        self.blitz_pages.retain(|key, _| !key.starts_with(prefix));
        self.blitz_order
            .retain(|key| self.blitz_pages.contains_key(key));
        if self
            .pending_blitz_key
            .as_ref()
            .is_some_and(|(key, _)| key.starts_with(prefix))
        {
            self.pending_blitz_key = None;
        }
    }
    /// Ask a worker thread to fetch a remote mail image (once per URL).
    pub(super) fn request_image(&mut self, url: String) {
        if self.images.contains_key(&url) {
            return; // already loading, loaded or failed
        }
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            self.images.insert(url, ImageStatus::Failed);
            return;
        }
        self.images.insert(url.clone(), ImageStatus::Loading);
        sync::spawn(Job::FetchImage { url }, self.ctx.clone(), self.tx.clone());
    }
    /// Ask a worker thread to fetch a remote stylesheet (once per URL).
    pub(super) fn request_stylesheet(&mut self, url: String) {
        if self.stylesheets.contains_key(&url) {
            return;
        }
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            self.stylesheets.insert(url.clone(), None);
            return;
        }
        self.stylesheets.insert(url.clone(), None);
        sync::spawn(
            Job::FetchStylesheet { url },
            self.ctx.clone(),
            self.tx.clone(),
        );
    }
    /// Identity of a rendered page: the mail it shows. Two pages with the
    /// same identity are the same document, rendered at different widths.
    pub(super) fn blitz_identity(&self, msg: &MailMessage) -> String {
        format!(
            "{}/{}/{}/",
            self.selected_account.as_deref().unwrap_or("-"),
            self.selected_folder.as_deref().unwrap_or("-"),
            msg.uid
        )
    }
    /// Ask a worker thread to rasterize this message's HTML with Blitz.
    ///
    /// Rendering a newsletter takes hundreds of milliseconds, so it never
    /// happens on the UI thread (hard rule 3); the finished page arrives as
    /// `Event::HtmlRendered`.
    pub(super) fn start_blitz_render(
        &mut self,
        key: String,
        msg: &MailMessage,
        html: &str,
        width: u32,
        viewport_height: u32,
        scale: f32,
    ) {
        // A new request cancels whatever render is still running (its ticket
        // stops being current, see `blitz_render::Cancellation`), and the
        // Loading entry it left behind goes with it.
        let ticket = self.blitz_cancel.issue();
        let stale: Vec<String> = self
            .blitz_pages
            .iter()
            .filter(|(_, page)| matches!(page, BlitzPage::Loading))
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            self.blitz_pages.remove(&key);
        }
        self.blitz_order
            .retain(|entry| self.blitz_pages.contains_key(entry));

        let request = crate::blitz_render::PageRequest {
            html: html.to_string(),
            width,
            viewport_height,
            scale,
            inline_images: msg
                .inline_images
                .iter()
                .map(|image| (image.cid.clone(), image.data.clone()))
                .collect(),
            ticket,
        };
        self.pending_blitz_key = None;
        self.blitz_pages.insert(key.clone(), BlitzPage::Loading);
        self.touch_blitz_page(&key);
        self.evict_blitz_pages();
        sync::spawn(
            Job::RenderHtml {
                key,
                request: Box::new(request),
            },
            self.ctx.clone(),
            self.tx.clone(),
        );
    }
    /// Turn a finished render into egui textures.
    ///
    /// Runs on the UI thread, but only uploads what the worker already
    /// rasterized. A result whose key is no longer pending (the user selected
    /// another message or resized the pane) is dropped.
    pub(super) fn store_blitz_page(&mut self, key: String, page: crate::blitz_render::Page) {
        if !matches!(self.blitz_pages.get(&key), Some(BlitzPage::Loading)) {
            return;
        }
        // The renderer may have rasterized at a lower scale than the one we
        // asked for (a page too large for its pixel budget), so sizes come
        // from the page itself, never from the requested scale.
        let to_points = |pixels: u32| pixels as f32 / page.scale;

        let mut tiles = Vec::with_capacity(page.tiles.len());
        for (index, tile) in page.tiles.iter().enumerate() {
            // vello renders premultiplied alpha, which is also how egui
            // stores and blends its textures.
            let image = egui::ColorImage::from_rgba_premultiplied(
                [tile.width as usize, tile.height as usize],
                &tile.rgba,
            );
            tiles.push(RasterTile {
                texture: self.ctx.load_texture(
                    format!("blitz:{key}:{index}"),
                    image,
                    egui::TextureOptions::LINEAR,
                ),
                offset: egui::vec2(0.0, to_points(tile.y)),
                size: egui::vec2(to_points(tile.width), to_points(tile.height)),
            });
        }
        let links = page
            .links
            .iter()
            .map(|link| {
                (
                    egui::Rect::from_min_size(
                        egui::pos2(link.x, link.y),
                        egui::vec2(link.width, link.height),
                    ),
                    link.href.clone(),
                )
            })
            .collect();
        self.blitz_pages.insert(
            key.clone(),
            BlitzPage::Ready(RasterizedPage {
                tiles,
                size: egui::vec2(page.css_width, page.css_height),
                links,
                truncated: page.truncated,
            }),
        );
        self.touch_blitz_page(&key);
        self.evict_blitz_pages();
    }
    /// Adopt a page already rendered for this mail when the pane wants a width
    /// it can simply be scaled down to (see `BLITZ_REUSE_RATIO`). Returns true
    /// when a page was adopted under `key`.
    ///
    /// The adopted entry is another handle on the same textures, not a copy of
    /// them, so this costs nothing next to a render.
    pub(super) fn reuse_blitz_page(
        &mut self,
        identity: &str,
        width: u32,
        scale: f32,
        key: &str,
    ) -> bool {
        let wanted = width as f32;
        let source = self.blitz_pages.iter().find_map(|(candidate, page)| {
            let BlitzPage::Ready(page) = page else {
                return None;
            };
            // The identity ends in '/', so a shorter uid cannot match a longer.
            let reusable =
                candidate.starts_with(identity) && blitz_page_reusable(page.size.x * scale, wanted);
            reusable.then(|| candidate.clone())
        });
        let Some(source) = source else {
            return false;
        };
        let Some(BlitzPage::Ready(page)) = self.blitz_pages.get(&source) else {
            return false;
        };
        let page = page.clone();
        self.blitz_pages
            .insert(key.to_string(), BlitzPage::Ready(page));
        self.pending_blitz_key = None;
        self.touch_blitz_page(key);
        self.evict_blitz_pages();
        true
    }
    /// Move `key` to the back of the cache order, so eviction always drops
    /// the least recently used page.
    pub(super) fn touch_blitz_page(&mut self, key: &str) {
        self.blitz_order.retain(|entry| entry != key);
        self.blitz_order.push(key.to_string());
    }
    /// Drop the oldest rendered pages; each one is a few megabytes of texture.
    pub(super) fn evict_blitz_pages(&mut self) {
        while self.blitz_order.len() > BLITZ_PAGES_CACHED {
            let oldest = self.blitz_order.remove(0);
            self.blitz_pages.remove(&oldest);
        }
    }
    /// Which paper this mail's Blitz page is drawn on: white unless the reader
    /// has switched this one to the dark surface.
    pub(super) fn paper_of(&self, key: &MailKey) -> Paper {
        self.blitz_paper.get(key).copied().unwrap_or_default()
    }
    /// Switch one mail's paper between white and the dark surface. Nothing else
    /// depends on it — not the render, whose key has no paper in it, and not the
    /// mail, whose own background the renderer paints over this either way — so
    /// the click only has to ask for the next frame.
    pub(super) fn toggle_blitz_paper(&mut self, key: &MailKey) {
        match self.paper_of(key) {
            Paper::White => {
                self.blitz_paper.insert(key.clone(), Paper::Dark);
            }
            Paper::Dark => {
                self.blitz_paper.remove(key);
            }
        }
    }
    /// Reading pane in Blitz mode: request a render if this message and
    /// geometry have none yet, then blit the finished page.
    pub(super) fn blitz_view(&mut self, ui: &mut egui::Ui, msg: &MailMessage, html: &str) {
        let scale = ui.ctx().pixels_per_point();
        let available = ui.available_width().max(160.0);
        let css_width = (available - BLITZ_PAGE_MARGIN * 2.0).max(160.0);
        let width = quantize(css_width * scale, BLITZ_WIDTH_STEP);
        // The pane height reaches Blitz's viewport (`vh` and friends) but not
        // the text's layout width, so it is sampled when a render starts
        // instead of being part of the key: a window resize must not make an
        // otherwise identical page render again.
        let viewport_height = (ui.available_height().max(160.0) * scale).round() as u32;
        let identity = self.blitz_identity(msg);
        let key = blitz_key(&identity, width, scale);

        if !self.blitz_pages.contains_key(&key)
            && !self.reuse_blitz_page(&identity, width, scale, &key)
        {
            // Let the geometry settle first: while it keeps changing (splitter
            // drag, animated window resize) no render is started at all. The
            // repaint request is what makes the render happen once the pointer
            // stops moving — egui has no other reason to draw another frame.
            let now = Instant::now();
            let held = self.pending_blitz_key.clone();
            let pending = held.as_ref().map(|(key, since)| (key.as_str(), *since));
            match blitz_settle(pending, &key, now) {
                BlitzWait::Start => {
                    self.start_blitz_render(key.clone(), msg, html, width, viewport_height, scale);
                }
                BlitzWait::Wait(remaining) => {
                    if held.as_ref().map(|(key, _)| key.as_str()) != Some(key.as_str()) {
                        // First sighting of this geometry: start the clock.
                        self.pending_blitz_key = Some((key.clone(), now));
                    }
                    ui.ctx().request_repaint_after(remaining);
                }
            }
        }

        let mut retry = false;
        // The paper is the pane's, not the page's: the render key has no paper
        // in it, so switching one repaints this frame and starts no work.
        let paper = self
            .pane_mail_key(&msg.uid)
            .map(|key| self.paper_of(&key))
            .unwrap_or_default();
        match self.blitz_pages.get(&key) {
            Some(BlitzPage::Ready(page)) => draw_blitz_page(ui, page, paper.color()),
            Some(BlitzPage::Failed(error)) => {
                ui.vertical_centered(|ui| {
                    ui.add_space(24.0);
                    ui.label(egui::RichText::new("Blitz could not render this mail").weak());
                    ui.label(egui::RichText::new(error.as_str()).small().weak());
                    retry = ui.button("Try again").clicked();
                });
            }
            _ => {
                ui.vertical_centered(|ui| {
                    ui.add_space(24.0);
                    ui.add(egui::Spinner::new().color(theme::of(ui.ctx()).accent));
                    ui.label(
                        egui::RichText::new("Rendering HTML with Blitz…")
                            .small()
                            .weak(),
                    );
                });
            }
        }
        if retry {
            self.blitz_pages.remove(&key);
            self.blitz_order.retain(|entry| entry != &key);
            self.pending_blitz_key = None;
        }
    }
}
