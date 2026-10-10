//! Full-fidelity HTML mail rendering with Blitz.
//!
//! [`crate::html`] is the built-in renderer: mail HTML becomes native egui
//! widgets and stays fully interactive. This module is the optional second
//! renderer, selectable from the title bar: the raw HTML body is handed to
//! Blitz — a pure-Rust browser engine (stylo for the cascade, taffy for
//! layout, vello_cpu for CPU rasterization) — and comes back as RGBA tiles
//! that the reading pane blits, plus the clickable link boxes.
//!
//! Rendering is synchronous and CPU-bound (a newsletter takes hundreds of
//! milliseconds), so it always runs on a `sync` worker thread
//! ([`crate::sync::Job::RenderHtml`]), never on the UI thread (hard rule 3).
//! Remote resources are fetched during the render by [`MailNet`], a blocking
//! `NetProvider` that reuses the app's on-disk resource cache, so mail that
//! was already displayed with the built-in renderer renders offline.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyrender::{ImageRenderer, PaintScene, Scene};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use base64::Engine as _;
use blitz_dom::{local_name, BaseDocument, DocumentConfig, FontContext, StyleThreading};
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request};
use blitz_traits::shell::{ColorScheme, Viewport};
use kurbo::Affine;
use parley::fontique::{
    Collection, CollectionOptions, FallbackKey, FamilyId, GenericFamily, Script, ScriptExt,
    SourceCache,
};

use crate::storage;

const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;
const BASE64_NO_PAD: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD_NO_PAD;

/// Every relative URL in a mail body is resolved against this. Blitz needs a
/// base that can be joined (its own default is a `data:` URL, which panics on
/// relative references), and HTML mail uses relative image URLs constantly.
/// The host is reserved by RFC 2606 and [`MailNet`] refuses to fetch from it,
/// so such references stay unresolved instead of hitting the network.
const BASE_URL: &str = "https://mail.jimao.invalid/";

/// Physical-pixel caps for one page: a pathological mail must not be able to
/// exhaust memory on a worker thread.
const MIN_WIDTH: u32 = 200;
const MAX_WIDTH: u32 = 4096;
/// Widest layout a fixed-width mail may be re-laid out at (CSS pixels) before
/// it is simply clipped.
const MAX_LAYOUT_WIDTH: f32 = 1280.0;
/// Horizontal overflow below this is layout rounding, not a fixed-width mail.
const OVERFLOW_TOLERANCE: f32 = 8.0;
/// Hard ceiling on a raster's height, whatever the scale.
const MAX_RASTER_HEIGHT: u32 = 24_000;
/// Ceiling on one page's raster, in pixels: 160 MB of textures, which is what
/// the reading pane ends up holding for the page. Long mail (a Confluence
/// excerpt runs past 8000 CSS px, ~29 MP at device scale 2) still fits; a page
/// past the ceiling is re-laid out at a lower scale rather than cut off,
/// because a missing tail is worse than a slightly soft page.
const MAX_PIXELS: u64 = 40_000_000;
/// Floor for that downscaling. Below it text stops being readable, so the
/// page is cut short instead and `Page::truncated` is set.
const MIN_RASTER_SCALE: f32 = 0.5;
/// Height of one texture tile, comfortably below every GPU's texture limit.
pub const TILE_HEIGHT: u32 = 2048;

/// How many times to re-run style/layout while resources trickle in.
const MAX_PASSES: usize = 16;

/// Network budget for a single render.
const FETCH_BUDGET: Duration = Duration::from_secs(20);
const MAX_RESOURCE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 48 * 1024 * 1024;

/// CSS layered on top of Blitz's own user-agent sheet, for things the pinned
/// engine gets wrong on real mail.
///
/// A cell's initial \`border-width\` is \`medium\` (3px) — invisible as long as
/// the initial \`border-style: none\` applies. Blitz's collapsed-border painter
/// (\`draw_table_borders\` in blitz-paint) reads the first cell's width without
/// checking its style, and paints the *table's* outer frame whenever the
/// table's own \`border-style\` is not \`hidden\` — which is true for \`none\`. Any
/// \`border-collapse: collapse\` table whose first cell declares no border
/// therefore gets a 3px frame in the table's text colour: a thick dark box
/// around every legacy layout table, which is most of mail HTML.
///
/// Zeroing the *initial* width for cells makes those phantom frames empty and
/// leaves declared borders untouched, since author rules and inline styles
/// outrank user-agent ones. The one visible difference is a cell styled with
/// \`border-style\` alone (and no width), which browsers draw 3px wide.
/// CSS layered on top of Blitz's own user-agent sheet, for things the pinned
/// engine gets wrong on real mail.
///
/// 1. Cell borders must not be painted by the *collapsed* border model.
///    Blitz implements border collapsing as "first cell's top border width and
///    colour applied to the whole table": the outer frame is painted whenever
///    the table's own \`border-style\` is not \`hidden\` (true for \`none\`), so a
///    \`border-collapse: collapse\` table whose first cell declares no border
///    gets a 3px frame in the table's text colour — the thick dark box around
///    every legacy layout table — while declared cell borders come out in the
///    wrong colour (a \`border-bottom: 1px solid #99aabb\` row separator renders
///    near-black). The separate model paints each cell's own borders with the
///    right width and colour, so collapsing is turned off. \`!important\` is
///    needed because mail sets it in inline styles, and a user-agent sheet
///    cannot otherwise outrank them.
/// 2. \`border-spacing\` defaults to 2px, and Blitz does not map the legacy
///    \`cellspacing\` attribute that mail tables use to zero it. Forcing zero
///    reproduces what those tables ask for; an author \`border-spacing\`
///    declaration still wins, since this rule is not marked important.
const COMPAT_CSS: &str = "\
table { border-collapse: separate !important; }
table { border-spacing: 0; }
";

/// A page to render: the raw HTML body plus the reading pane's geometry and
/// everything the message carries locally.
pub struct PageRequest {
    pub html: String,
    /// Reading-pane width in physical pixels.
    pub width: u32,
    /// Reading-pane height in physical pixels; drives `vh` units and
    /// percentage heights.
    pub viewport_height: u32,
    /// Device pixels per CSS pixel (`egui::Context::pixels_per_point`).
    pub scale: f32,
    /// Inline `cid:` parts of this message: `(Content-ID, base64 data)`.
    pub inline_images: Vec<(String, String)>,
}

/// The finished page, in CSS pixels, cut into texture-sized tiles.
pub struct Page {
    pub css_width: f32,
    pub css_height: f32,
    /// Device pixels per CSS pixel the page was rasterized at: the requested
    /// scale, unless the page was too large for the pixel budget and the
    /// document had to be re-laid out at a smaller one. The reading pane must
    /// divide tile pixels by this to get points.
    pub scale: f32,
    /// Top-to-bottom slices, each at most [`TILE_HEIGHT`] physical pixels tall.
    pub tiles: Vec<Tile>,
    /// Clickable link boxes, in CSS pixels relative to the page origin.
    pub links: Vec<LinkBox>,
    /// True when the page was too tall even for the smallest raster scale, so
    /// its bottom is missing (see `MIN_RASTER_SCALE`).
    pub truncated: bool,
}

pub struct Tile {
    /// Offset of this tile from the top of the page, in physical pixels.
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// RGBA (premultiplied, as vello renders it), `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

pub struct LinkBox {
    pub href: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Render `request` into a rasterized page. Blocking: call from a worker thread.
pub fn render(request: &PageRequest) -> Result<Page, String> {
    let width = request.width.clamp(MIN_WIDTH, MAX_WIDTH);
    let scale = if request.scale.is_finite() && request.scale > 0.0 {
        request.scale.clamp(0.5, 4.0)
    } else {
        1.0
    };
    let viewport_height = request.viewport_height.clamp(160, MAX_RASTER_HEIGHT);

    let net = Arc::new(MailNet::new(&request.inline_images));
    let mut doc = HtmlDocument::from_html(
        &request.html,
        DocumentConfig {
            viewport: Some(page_viewport(width, viewport_height, scale)),
            base_url: Some(BASE_URL.to_string()),
            // Blitz's default sheet first, then our compatibility rules;
            // `HtmlDocument` only appends its own copy if it is missing.
            ua_stylesheets: Some(vec![
                blitz_dom::DEFAULT_CSS.to_string(),
                COMPAT_CSS.to_string(),
            ]),
            net_provider: Some(net.clone() as Arc<dyn NetProvider>),
            font_ctx: Some(font_context()),
            style_threading: StyleThreading::Sequential,
            ..Default::default()
        },
    );
    settle(&mut doc, &net);

    // Mail built on a fixed-width table (the classic 600px newsletter) does
    // not reflow into a narrow reading pane, it overflows — and since the pane
    // only scrolls vertically, everything past the right edge would be lost.
    // Lay such mail out at its own width instead and let the reading pane
    // scale the finished page down.
    let mut layout_width = width as f32 / scale;
    let (laid_out_width, mut content_height) = document_size(&doc);
    if laid_out_width > layout_width + OVERFLOW_TOLERANCE && laid_out_width <= MAX_LAYOUT_WIDTH {
        layout_width = laid_out_width;
        doc.set_viewport(page_viewport(
            (layout_width * scale).round() as u32,
            viewport_height,
            scale,
        ));
        settle(&mut doc, &net);
        content_height = document_size(&doc).1;
    }

    // The painter's scale must be the scale the document was laid out at:
    // blitz builds text layout at the *viewport* scale and positions glyphs
    // with the *painter's* scale, so painting a document at a lower scale than
    // it was resolved for renders oversized text that overflows its box. A
    // page too large for the pixel budget is therefore re-laid out at the
    // smaller scale rather than merely painted smaller.
    let css_viewport_height = viewport_height as f32 / scale;
    let raster_scale = affordable_scale(layout_width, content_height, scale);
    if raster_scale < scale {
        doc.set_viewport(page_viewport(
            (layout_width * raster_scale).round() as u32,
            (css_viewport_height * raster_scale).round() as u32,
            raster_scale,
        ));
        settle(&mut doc, &net);
        content_height = document_size(&doc).1;
    }
    let raster_width = ((layout_width * raster_scale).round() as u32).clamp(MIN_WIDTH, MAX_WIDTH);
    // The clamp above can nudge the width, so derive the scale actually in
    // use from the width that survived it.
    let raster_scale = raster_width as f32 / layout_width;
    let wanted_height = (content_height * raster_scale).ceil().max(1.0) as u32;
    let raster_height = wanted_height
        .min(MAX_RASTER_HEIGHT)
        .min((MAX_PIXELS as u32).div_ceil(raster_width.max(1)));
    let truncated = raster_height < wanted_height;
    if truncated {
        // Rare enough to be worth a note: even the floor scale did not fit.
        eprintln!("blitz_render: page cut off, {layout_width:.0}x{content_height:.0} CSS px");
    }

    // Record the page once, then rasterize it one slice at a time. A single
    // surface would need width*height*4 bytes up front — hundreds of megabytes
    // for a long mail — while a recorded scene is just draw commands, and
    // replaying it with a translation lets vello rasterize any slice of it.
    let mut scene = Scene::new();
    {
        let base: &mut BaseDocument = &mut doc;
        paint_scene(
            &mut scene,
            base,
            raster_scale as f64,
            raster_width,
            raster_height,
            0,
            0,
        );
    }

    let mut tiles = Vec::new();
    let mut y = 0;
    while y < raster_height {
        let slice_height = TILE_HEIGHT.min(raster_height - y);
        let mut renderer = VelloCpuImageRenderer::new(raster_width, slice_height);
        let mut rgba = Vec::new();
        let slice = scene.clone();
        renderer.render_to_vec(
            |painter| painter.append_scene(slice, Affine::translate((0.0, -(y as f64)))),
            &mut rgba,
        );
        tiles.push(Tile {
            y,
            width: raster_width,
            height: slice_height,
            rgba,
        });
        y += slice_height;
    }

    Ok(Page {
        // What the reading pane needs: the page's size in points.
        css_width: raster_width as f32 / raster_scale,
        css_height: raster_height as f32 / raster_scale,
        scale: raster_scale,
        tiles,
        links: collect_links(&doc, raster_scale),
        truncated,
    })
}

/// The scale a page of `css_width` × `css_height` CSS pixels is rasterized
/// at: the requested one, or the largest that fits [`MAX_PIXELS`], never below
/// [`MIN_RASTER_SCALE`].
///
/// Reducing this means re-laying the document out at that scale, not merely
/// painting it smaller — see [`render`].
fn affordable_scale(css_width: f32, css_height: f32, scale: f32) -> f32 {
    // 0.99: sit just inside the budget so rounding cannot push the last row out.
    let affordable = ((MAX_PIXELS as f32 * 0.99) / (css_width * css_height.max(1.0))).sqrt();
    scale.min(affordable).max(MIN_RASTER_SCALE)
}

fn page_viewport(width: u32, height: u32, scale: f32) -> Viewport {
    Viewport::new(width.max(1), height.max(1), scale, ColorScheme::Light)
}

/// Run style/layout until the document stops discovering sub-resources.
///
/// Blitz runs a browser-style loop: style/layout discovers resources, hands
/// them to the net provider, and picks the answers up on the next pass
/// (`handle_messages` at the top of `resolve`). MailNet answers synchronously,
/// so a few passes settle the document; stop as soon as a pass neither left a
/// render-blocking resource pending nor asked for anything new.
fn settle(doc: &mut BaseDocument, net: &MailNet) {
    for _ in 0..MAX_PASSES {
        let before = net.requests();
        doc.resolve(0.0);
        if !doc.has_pending_critical_resources() && net.requests() == before {
            break;
        }
    }
}

/// The document's size in CSS pixels, including overflowing content.
fn document_size(doc: &BaseDocument) -> (f32, f32) {
    match doc.try_root_element() {
        Some(root) => (root.scroll_width(), root.scroll_height()),
        None => (0.0, 0.0),
    }
}

/// Walk every `a[href]` in the document and record its on-page box, so the
/// reading pane can make the rasterized page clickable again.
fn collect_links(doc: &BaseDocument, scale: f32) -> Vec<LinkBox> {
    let Ok(ids) = doc.query_selector_all("a[href]") else {
        return Vec::new();
    };
    let mut links = Vec::new();
    for id in ids {
        let Some(node) = doc.get_node(id) else {
            continue;
        };
        let Some(href) = node.attr(local_name!("href")) else {
            continue;
        };
        let href = href.trim();
        // In-page anchors need no browser; everything else opens externally.
        if href.is_empty() || href.starts_with('#') {
            continue;
        }
        let Some(rect) = doc.get_client_bounding_rect(id) else {
            continue;
        };
        if rect.width < 1.0 || rect.height < 1.0 {
            continue;
        }
        links.push(LinkBox {
            href: href.to_string(),
            x: (rect.x / scale as f64) as f32,
            y: (rect.y / scale as f64) as f32,
            width: (rect.width / scale as f64) as f32,
            height: (rect.height / scale as f64) as f32,
        });
    }
    links
}

// ----- resource loading ------------------------------------------------------

/// The blocking [`NetProvider`] behind one render.
///
/// Blitz asks for every sub-resource here: linked stylesheets, `@import`s,
/// `@font-face` fonts, `<img>` sources (including `data:` URLs) and `cid:`
/// inline parts. Answers are delivered synchronously, which is what lets
/// [`render`] settle in a handful of passes; the render already runs on a
/// worker thread, so blocking on a socket is fine (rules 2 and 3).
struct MailNet {
    /// `Content-ID` to decoded bytes, for `cid:` references.
    inline: HashMap<String, Vec<u8>>,
    requests: AtomicUsize,
    budget: Mutex<Budget>,
}

struct Budget {
    bytes_left: u64,
    deadline: Instant,
}

impl MailNet {
    fn new(inline_images: &[(String, String)]) -> Self {
        let mut inline = HashMap::new();
        for (cid, data) in inline_images {
            if let Some(bytes) = decode_base64(data) {
                inline.insert(cid.clone(), bytes);
            }
        }
        Self {
            inline,
            requests: AtomicUsize::new(0),
            budget: Mutex::new(Budget {
                bytes_left: MAX_TOTAL_BYTES,
                deadline: Instant::now() + FETCH_BUDGET,
            }),
        }
    }

    /// How many requests have been handed out so far; the render loop uses
    /// this to notice that a pass stopped discovering resources.
    fn requests(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }

    /// Resolve one request to bytes. Never fails: an unavailable resource is
    /// an empty body, which Blitz reports as a broken image or an empty
    /// stylesheet. (A provider that never called back would leave
    /// render-blocking stylesheets pending forever.)
    fn load(&self, url: &str) -> Vec<u8> {
        if let Some(data) = url.strip_prefix("data:") {
            return decode_data_url(data).unwrap_or_default();
        }
        if let Some(cid) = url.strip_prefix("cid:") {
            let cid = String::from_utf8_lossy(&percent_decode(cid)).into_owned();
            return self.inline.get(&cid).cloned().unwrap_or_default();
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) || url.starts_with(BASE_URL)
        {
            return Vec::new();
        }
        if let Some(cached) = storage::load_resource(url) {
            return cached;
        }
        let allowance = {
            let budget = match self.budget.lock() {
                Ok(budget) => budget,
                Err(_) => return Vec::new(),
            };
            if budget.bytes_left == 0 || Instant::now() >= budget.deadline {
                return Vec::new();
            }
            budget.bytes_left.min(MAX_RESOURCE_BYTES)
        };
        let Ok(bytes) = crate::sync::http_get(url, allowance) else {
            return Vec::new();
        };
        if let Ok(mut budget) = self.budget.lock() {
            budget.bytes_left = budget.bytes_left.saturating_sub(bytes.len() as u64);
        }
        storage::save_resource(url, &bytes);
        bytes
    }
}

impl NetProvider for MailNet {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let url = request.url.as_str().to_string();
        self.requests.fetch_add(1, Ordering::Relaxed);
        let bytes = self.load(&url);
        handler.bytes(url, Bytes::from(bytes));
    }
}

/// Decode a `data:` URL payload (`data:[<mediatype>][;base64],<data>`).
fn decode_data_url(data: &str) -> Option<Vec<u8>> {
    let (meta, payload) = data.split_once(',')?;
    if meta
        .rsplit(';')
        .next()
        .is_some_and(|flag| flag.eq_ignore_ascii_case("base64"))
    {
        decode_base64(payload)
    } else {
        Some(percent_decode(payload))
    }
}

/// Decode base64 leniently: mail HTML is full of line-wrapped, unpadded
/// payloads that the strict engine rejects.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let cleaned: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    BASE64
        .decode(&cleaned)
        .or_else(|_| BASE64_NO_PAD.decode(cleaned.trim_end_matches('=')))
        .ok()
}

/// Decode `%XX` escapes; invalid escapes are passed through verbatim.
fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                out.push(high * 16 + low);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// ----- fonts -----------------------------------------------------------------

/// The fonts Blitz lays text out with, loaded once per process and shared by
/// every render (the collection is made shareable before it is handed out).
///
/// Blitz's own `system-fonts` feature enumerates fonts through fontconfig — a
/// C library, which hard rule 1 forbids — so the dependency is built without
/// it and the font files are registered here with parley/fontique, the same
/// best-effort approach `app.rs::install_cjk_font` takes for egui.
fn font_context() -> FontContext {
    static FONTS: OnceLock<FontContext> = OnceLock::new();
    FONTS.get_or_init(load_fonts).clone()
}

fn load_fonts() -> FontContext {
    let mut context = FontContext {
        collection: Collection::new(CollectionOptions {
            shared: false,
            system_fonts: false,
        }),
        source_cache: SourceCache::new_shared(),
    };
    context
        .collection
        .load_fonts_from_paths(discover_font_files());
    tune_families(&mut context.collection);
    context.source_cache.make_shared();
    context.collection.make_shared();
    context
}

/// Font directories worth scanning, in preference order. Missing ones are
/// skipped, so this needs no configuration on any Unix.
const FONT_DIRS: &[&str] = &[
    "/usr/share/fonts",
    "/usr/local/share/fonts",
    "/usr/share/X11/fonts",
    "/System/Library/Fonts",
    "/Library/Fonts",
    "/Library/Fonts/Microsoft",
];

const FONT_EXTENSIONS: &[&str] = &["ttf", "otf", "ttc", "otc"];

/// Font files worth registering, matched on the file name with punctuation
/// removed ("DejaVu Sans" and "DejaVuSans" both become "dejavusans").
///
/// Registering every installed font would be more faithful, but parsing a
/// font's tables costs milliseconds even in release builds: on a machine with
/// ~1000 installed faces the scan dominated the first Blitz render. A curated
/// set keeps that one-off cost small and still covers the families mail
/// actually asks for — and the rest of the fallback chain below is built from
/// whatever did get registered.
const PREFERRED_FONTS: &[&str] = &[
    "liberationsans",
    "liberationserif",
    "liberationmono",
    "dejavusans",
    "dejavuserif",
    "dejavusansmono",
    "notosansregular",
    "notosansbold",
    "notosansitalic",
    "notoserifregular",
    "notoserifbold",
    "notoserifitalic",
    "notosansmono",
    "notosansarabic",
    "notosanshebrew",
    "notosansthai",
    "notosansdevanagari",
    "notosanssymbols",
    "notocoloremoji",
    "notoemoji",
    "carlito",
    "caladea",
    "cantarell",
    // GNU FreeFont is the classic last-resort face on Linux and covers
    // scripts the families above miss (Thai, Devanagari, …); it lands at the
    // end of the fallback chain, where coverage matters more than looks.
    "freesans",
    "freeserif",
    "freemono",
    "arial",
    "timesnewroman",
    "couriernew",
    "georgia",
    "verdana",
    "tahoma",
];

/// CJK families, restricted to Regular and Bold: their collections are ~20 MB
/// files with several faces each, and parsing every weight would cost seconds.
const PREFERRED_CJK_FONTS: &[&str] = &[
    "notosanscjk",
    "notoserifcjk",
    "sourcehansans",
    "sourcehanserif",
    "notosanssc",
    "notosanstc",
    "notosansjp",
    "notosanskr",
    "notoserifsc",
    "notoseriftc",
    "notoserifjp",
    "notoserifkr",
    "wqymicrohei",
    "wqyzenhei",
];

/// Cap on the last-resort scan when none of the preferred fonts are installed.
const FALLBACK_FONT_LIMIT: usize = 48;

fn discover_font_files() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = FONT_DIRS.iter().map(PathBuf::from).collect();
    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".fonts"));
        roots.push(home.join(".local/share/fonts"));
    }
    if let Ok(data) = std::env::var("XDG_DATA_HOME") {
        if !data.is_empty() {
            roots.push(PathBuf::from(data).join("fonts"));
        }
    }

    let mut installed = Vec::new();
    for root in roots {
        collect_fonts(&root, 4, &mut installed);
    }
    installed.sort();
    installed.dedup();

    let preferred: Vec<PathBuf> = installed
        .iter()
        .filter(|path| is_preferred_font(path))
        .cloned()
        .collect();
    if !preferred.is_empty() {
        return preferred;
    }
    // Nothing familiar installed: fall back to a bounded slice of whatever is
    // there rather than rendering every glyph as tofu.
    installed.truncate(FALLBACK_FONT_LIMIT);
    installed
}

/// Match a font file against `PREFERRED_FONTS` / `PREFERRED_CJK_FONTS`.
fn is_preferred_font(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    let name: String = stem
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect();
    if PREFERRED_FONTS.iter().any(|family| name.contains(family)) {
        return true;
    }
    PREFERRED_CJK_FONTS
        .iter()
        .any(|family| name.contains(family))
        && (name.contains("regular") || name.contains("bold"))
}

fn collect_fonts(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => collect_fonts(&path, depth - 1, out),
            Ok(kind) if kind.is_file() => {
                let is_font = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        FONT_EXTENSIONS
                            .iter()
                            .any(|known| ext.eq_ignore_ascii_case(known))
                    });
                if is_font {
                    out.push(path);
                }
            }
            _ => {}
        }
    }
}

/// Families tried, in order, for the generic CSS families. Latin faces come
/// first so the common case stops early; CJK faces follow because a Chinese
/// mail usually asks for a Latin family and relies on fallback for the Han
/// glyphs.
const PREFERRED_SANS: &[&str] = &[
    "Noto Sans",
    "DejaVu Sans",
    "Liberation Sans",
    "Arial",
    "Helvetica",
    "Inter",
    "Carlito",
    "Segoe UI",
    "Roboto",
    "Noto Sans CJK SC",
    "Noto Sans CJK TC",
    "Noto Sans CJK JP",
    "Noto Sans CJK KR",
    "Source Han Sans SC",
    "WenQuanYi Micro Hei",
    "WenQuanYi Zen Hei",
    "Noto Sans Arabic",
    "Noto Sans Hebrew",
    "Noto Sans Thai",
    "Noto Sans Devanagari",
    "Noto Sans Symbols",
    "Noto Sans Symbols 2",
    "Noto Color Emoji",
    "Noto Emoji",
];

const PREFERRED_SERIF: &[&str] = &[
    "Noto Serif",
    "DejaVu Serif",
    "Liberation Serif",
    "Times New Roman",
    "Georgia",
    "Caladea",
    "Noto Serif CJK SC",
    "Noto Serif CJK TC",
    "Noto Serif CJK JP",
    "Noto Serif CJK KR",
    "Source Han Serif SC",
];

const PREFERRED_MONO: &[&str] = &[
    "Noto Sans Mono",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Courier New",
    "Source Code Pro",
    "Noto Sans Mono CJK SC",
    "WenQuanYi Zen Hei Mono",
];

/// Give the CSS generic families and the per-script fallbacks real targets.
///
/// With `system_fonts: false` fontique starts out empty-handed and parley
/// resolves `font-family` by name only: a mail asking for "Verdana" (or for
/// `sans-serif`, or for nothing at all) would otherwise end up with an empty
/// family list. Parley shapes with the `script` fallback list in that case, so
/// every script fontique knows about gets the same chain here, ending in every
/// registered family as a last resort.
fn tune_families(collection: &mut Collection) {
    let sans = family_ids(collection, PREFERRED_SANS);
    let serif = family_ids(collection, PREFERRED_SERIF);
    let mono = family_ids(collection, PREFERRED_MONO);

    for (generic, families) in [
        (GenericFamily::SansSerif, &sans),
        (GenericFamily::Serif, &serif),
        (GenericFamily::Monospace, &mono),
        (GenericFamily::Cursive, &sans),
        (GenericFamily::Fantasy, &sans),
        (GenericFamily::SystemUi, &sans),
        (GenericFamily::UiSansSerif, &sans),
        (GenericFamily::UiSerif, &serif),
        (GenericFamily::UiMonospace, &mono),
        (GenericFamily::UiRounded, &sans),
        (GenericFamily::Emoji, &sans),
        (GenericFamily::Math, &serif),
        (GenericFamily::FangSong, &serif),
    ] {
        if !families.is_empty() {
            collection.set_generic_families(generic, families.iter().copied());
        }
    }

    let mut chain = sans;
    chain.extend(serif);
    chain.extend(mono);
    let known: Vec<String> = collection.family_names().map(str::to_string).collect();
    for name in &known {
        if let Some(id) = collection.family_id(name) {
            if !chain.contains(&id) {
                chain.push(id);
            }
        }
    }
    if chain.is_empty() {
        return;
    }
    for (script, _) in Script::all_samples() {
        collection.set_fallbacks(FallbackKey::new(*script, None), chain.iter().copied());
    }
}

fn family_ids(collection: &mut Collection, names: &[&str]) -> Vec<FamilyId> {
    names
        .iter()
        .filter_map(|name| collection.family_id(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding_handles_escapes_and_passthrough() {
        assert_eq!(percent_decode("a%20b"), b"a b");
        assert_eq!(percent_decode("%E4%B8%AD"), "中".as_bytes());
        // Invalid escapes survive untouched.
        assert_eq!(percent_decode("100%"), b"100%");
        assert_eq!(percent_decode("%zz"), b"%zz");
    }

    #[test]
    fn data_urls_decode_in_both_encodings() {
        // "PNG" base64-encoded, with the whitespace real mail contains.
        let decoded = decode_data_url("image/png;base64,UE 5H").expect("base64 payload");
        assert_eq!(decoded, b"PNG");
        let decoded = decode_data_url("text/plain,hello%20world").expect("plain payload");
        assert_eq!(decoded, b"hello world");
        assert!(decode_data_url("image/png;base64").is_none());
    }

    #[test]
    fn cid_parts_reach_the_net_provider() {
        let net = MailNet::new(&[("logo@example.com".into(), "UE5H".into())]);
        assert_eq!(net.load("cid:logo@example.com"), b"PNG");
        assert_eq!(net.load("cid:missing@example.com"), Vec::<u8>::new());
        // Reserved base host: relative references are never fetched.
        assert_eq!(
            net.load(&format!("{BASE_URL}img/logo.png")),
            Vec::<u8>::new()
        );
    }

    /// End to end: a page with a solid background must come back as painted
    /// pixels, and its links must be reported so the UI can make them
    /// clickable again.
    #[test]
    fn renders_html_to_tiles_with_links() {
        let page = render(&PageRequest {
            html: "<body style=\"margin:0\">\
                   <p style=\"margin:0;width:400px;height:40px;background:#ff0000\"></p>\
                   <a href=\"https://example.com/inbox\">open</a></body>"
                .to_string(),
            width: 400,
            viewport_height: 300,
            scale: 1.0,
            inline_images: Vec::new(),
        })
        .expect("render");

        assert!(page.css_width > 0.0 && page.css_height > 0.0);
        let painted = page
            .tiles
            .iter()
            .flat_map(|tile| tile.rgba.as_chunks::<4>().0)
            .filter(|px| px[0] > 200 && px[1] < 60 && px[2] < 60 && px[3] > 200)
            .count();
        assert!(
            painted > 100,
            "expected red background pixels, got {painted}"
        );
        let covered: u32 = page.tiles.iter().map(|tile| tile.height).sum();
        assert_eq!(covered, page.css_height.ceil() as u32);
        assert!(
            page.links
                .iter()
                .any(|link| link.href == "https://example.com/inbox"),
            "links: {:?}",
            page.links.iter().map(|l| &l.href).collect::<Vec<_>>()
        );
    }

    /// How the raster scale reacts to the pixel budget.
    #[test]
    fn raster_scale_shrinks_only_when_needed() {
        // A typical mail keeps the requested scale (a 900x4000 CSS px page).
        assert_eq!(affordable_scale(900.0, 4000.0, 2.0), 2.0);
        // 900x8000 CSS px at 2x is 28.8 MP: still inside the budget.
        assert_eq!(affordable_scale(900.0, 8000.0, 2.0), 2.0);
        // 900x32000 CSS px would need 115 MP, so the scale drops — but the
        // page still fits the budget and stays above the floor.
        let scale = affordable_scale(900.0, 32_000.0, 2.0);
        assert!(scale < 2.0 && scale > MIN_RASTER_SCALE, "{scale}");
        assert!(900.0 * 32_000.0 * scale * scale <= MAX_PIXELS as f32);
        // A pathological page hits the floor instead of shrinking to nothing.
        assert_eq!(affordable_scale(900.0, 4_000_000.0, 2.0), MIN_RASTER_SCALE);
    }

    /// A long mail keeps its tail, at full resolution, rasterized in slices
    /// (nothing may depend on one surface holding the whole page).
    #[test]
    fn long_pages_keep_their_tail() {
        let mut html = String::from("<body style=\"margin:0\">");
        for i in 0..40 {
            html.push_str(&format!(
                "<div style=\"height:200px;background:#eee\">{i}</div>"
            ));
        }
        html.push_str("</body>");

        // 900 CSS px wide by ~8000 CSS px tall at device scale 2: inside the
        // pixel budget, so it must come back at full resolution.
        let page = render(&PageRequest {
            html,
            width: 1800,
            viewport_height: 600,
            scale: 2.0,
            inline_images: Vec::new(),
        })
        .expect("render");

        assert!(!page.truncated, "the tail must survive the pixel budget");
        assert_eq!(page.scale, 2.0, "no downscaling was needed");
        assert!(
            page.css_height >= 8000.0,
            "page reported {} CSS px",
            page.css_height
        );
        // Sliced, and the slices tile the page contiguously.
        assert!(page.tiles.len() > 1, "expected several slices");
        let mut next = 0;
        for tile in &page.tiles {
            assert_eq!(tile.y, next, "slices must be contiguous");
            next += tile.height;
        }
        let raster_width = page.tiles[0].width;
        let raster_height: u32 = page.tiles.iter().map(|tile| tile.height).sum();
        assert!(
            raster_width as u64 * raster_height as u64 <= MAX_PIXELS,
            "raster {raster_width}x{raster_height} exceeds the budget"
        );
        // The last row of the raster is inside the last block, so it must be
        // painted rather than empty.
        let last = page.tiles.last().expect("tiles");
        let bottom = &last.rgba[last.rgba.len() - last.width as usize * 4..];
        assert!(
            bottom.as_chunks::<4>().0.iter().any(|px| px[3] > 0),
            "the bottom of the page is empty"
        );
    }

    /// Developer tool: rasterize an HTML file to PNG so the result can be
    /// eyeballed (the UI has no golden test).
    ///
    ///     JIMAO_RENDER_HTML=mail.html cargo test -- --ignored dump_render
    ///
    /// `JIMAO_RENDER_WIDTH` and `JIMAO_RENDER_SCALE` (device pixels per CSS
    /// pixel) mirror the reading pane's geometry.
    #[test]
    #[ignore = "developer tool; needs JIMAO_RENDER_HTML"]
    fn dump_render() {
        let Some(path) = std::env::var_os("JIMAO_RENDER_HTML") else {
            return;
        };
        let html = std::fs::read_to_string(&path).expect("read html");
        let width = std::env::var("JIMAO_RENDER_WIDTH")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1000);
        let scale = std::env::var("JIMAO_RENDER_SCALE")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1.0);
        let page = render(&PageRequest {
            html,
            width,
            viewport_height: 800,
            scale,
            inline_images: Vec::new(),
        })
        .expect("render");
        println!(
            "page: css {:.0}x{:.0}, raster {}x{}, tiles {}, truncated {}",
            page.css_width,
            page.css_height,
            page.tiles[0].width,
            page.tiles.iter().map(|t| t.height).sum::<u32>(),
            page.tiles.len(),
            page.truncated
        );
        let raster_height: u32 = page.tiles.iter().map(|tile| tile.height).sum();
        let mut image = image::RgbaImage::new(page.tiles[0].width, raster_height);
        for tile in &page.tiles {
            let slice = image::RgbaImage::from_raw(tile.width, tile.height, tile.rgba.clone())
                .expect("tile dimensions");
            image::imageops::overlay(&mut image, &slice, 0, tile.y as i64);
        }
        let out = std::path::Path::new(&path).with_extension("png");
        image.save(&out).expect("write png");
        println!(
            "wrote {} ({}x{})",
            out.display(),
            image.width(),
            image.height()
        );
    }
}
