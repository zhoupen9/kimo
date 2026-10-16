//! Renders HTML mail bodies as egui widgets.
//!
//! Parsing is done by html5ever (via `scraper`), so the input is treated
//! exactly like a browser would treat it (error recovery, entity decoding,
//! implied tbody, …). Rendering supports the practical subset of HTML mail:
//! headings, paragraphs, lists, tables, blockquotes, pre/code, inline
//! styling, links, and a small CSS subset (see `src/css.rs`). Remote images
//! and stylesheets load through the app's fetch pipeline: the renderer
//! reports missing URLs in `HtmlContext` and displays whatever textures it
//! is given. There is still no JavaScript.

use std::collections::HashMap;

use ego_tree::NodeRef;
use egui::{RichText, Ui};
use scraper::{Html, Node};

use crate::css;
use crate::theme;

/// Remote and inline resources available to the renderer plus the URLs it
/// wants fetched. The app supplies what it has (loaded textures, fetched
/// stylesheets, and the selected message's `cid:` inline images) and turns
/// the missing lists into `Job::FetchImage` / `FetchStylesheet`.
pub struct HtmlContext<'a> {
    pub images: &'a HashMap<String, egui::TextureHandle>,
    /// Inline attachments of the message being rendered, keyed by
    /// Content-ID (no angle brackets) — the `cid:` reference targets.
    pub inline_images: &'a HashMap<String, egui::TextureHandle>,
    pub stylesheets: &'a HashMap<String, String>,
    pub missing_images: Vec<String>,
    pub missing_stylesheets: Vec<String>,
}

/// One styled run of text; consecutive runs with equal styling are merged.
#[derive(Clone, Default, PartialEq)]
struct Seg {
    text: String,
    bold: bool,
    italic: bool,
    underline: bool,
    mono: bool,
    weak: bool,
    size: Option<f32>,
    color: Option<egui::Color32>,
    background: Option<egui::Color32>,
    link: Option<String>,
}

/// A parsed mail body, ready to be laid out as often as the pane wants.
///
/// Parsing is html5ever's and it is not cheap: on a 100 KB newsletter it is
/// about two thirds of what the built-in renderer costs per frame, and the
/// reading pane draws the same body on every frame. So the pane parses once
/// per body — `app.rs` keeps one of these — and re-walks the tree each frame,
/// which is what actually lays the mail out.
pub struct Document(Html);

impl Document {
    pub fn parse(html: &str) -> Self {
        Self(Html::parse_fragment(html))
    }
}

/// Parse `html` and lay it out into `ui` as flowing blocks.
///
/// Test-only: a pane that draws the same body every frame parses it once
/// ([`Document::parse`]) and lays that out with [`render_document`], which is
/// what `app.rs` does.
#[cfg(test)]
pub fn render(ui: &mut Ui, html: &str, html_ctx: &mut HtmlContext<'_>) {
    render_document(ui, &Document::parse(html), html_ctx);
}

/// Lay out a body [`Document::parse`] has already parsed.
pub fn render_document(ui: &mut Ui, doc: &Document, html_ctx: &mut HtmlContext<'_>) {
    let mut r = Renderer {
        pending: Vec::new(),
        ids: 0,
        // The mail's own colours are drawn for the paper it was written on, so
        // the renderer has to know which surface it is drawing on
        // (`adapt_color`). Read once per layout: it cannot change mid-walk.
        dark: theme::of(ui.ctx()).dark,
        rules: html_ctx
            .stylesheets
            .values()
            .flat_map(|text| css::parse_stylesheet(text))
            .collect(),
        ctx: html_ctx,
    };
    for node in doc.0.tree.root().children() {
        r.node(ui, node, &Seg::default());
    }
    r.flush(ui);
}

struct Renderer<'r, 'a> {
    pending: Vec<Seg>,
    ids: u64,
    /// Whether the theme in force is the dark one, which is what the mail's own
    /// colours are adapted to (`adapt_color`).
    dark: bool,
    /// Parsed CSS rules: fetched stylesheets first, then `<style>` blocks
    /// in document order.
    rules: Vec<css::Rule>,
    ctx: &'r mut HtmlContext<'a>,
}

impl Renderer<'_, '_> {
    fn next_id(&mut self, prefix: &str) -> egui::Id {
        self.ids += 1;
        egui::Id::new(format!("{prefix}-{}", self.ids))
    }

    fn text(&mut self, text: &str, style: &Seg) {
        if text.is_empty() {
            return;
        }
        let text = if self.pending.is_empty() {
            text.trim_start_matches(' ')
        } else {
            text
        };
        if text.is_empty() {
            return;
        }
        match self.pending.last_mut() {
            Some(last) if last.style_like() == style.style_like() => last.text.push_str(text),
            _ => {
                let mut seg = style.clone();
                seg.text = text.to_string();
                self.pending.push(seg);
            }
        }
    }

    fn newline(&mut self, style: &Seg) {
        match self.pending.last_mut() {
            Some(last) if last.style_like() == style.style_like() => last.text.push('\n'),
            _ => {
                let mut seg = style.clone();
                seg.text = "\n".to_string();
                self.pending.push(seg);
            }
        }
    }

    /// Lay out the accumulated inline runs as one wrapped block.
    fn flush(&mut self, ui: &mut Ui) {
        if self.pending.is_empty() {
            return;
        }
        let segs = std::mem::take(&mut self.pending);
        ui.horizontal_wrapped(|ui| {
            for seg in segs {
                let rich = style_rich(&seg);
                match seg.link {
                    Some(url) => {
                        if ui.add(egui::Link::new(rich)).clicked() {
                            open_url(ui, &url);
                        }
                    }
                    None => {
                        ui.label(rich);
                    }
                }
            }
        });
    }

    fn children(&mut self, ui: &mut Ui, node: NodeRef<'_, Node>, style: &Seg) {
        for child in node.children() {
            self.node(ui, child, style);
        }
    }

    /// Recurse into children with a modified copy of the current style.
    fn children_with(
        &mut self,
        ui: &mut Ui,
        node: NodeRef<'_, Node>,
        style: &Seg,
        f: impl FnOnce(&mut Seg),
    ) {
        let styled = with(style, f);
        self.children(ui, node, &styled);
    }

    /// Overlay the CSS declarations that target this element.
    fn decls_for(&self, el: &scraper::node::Element, name: &str) -> css::Decls {
        let mut decls = css::Decls::default();
        let classes: Vec<&str> = el
            .attr("class")
            .map(|c| c.split_whitespace().collect())
            .unwrap_or_default();
        let id = el.attr("id");
        for rule in &self.rules {
            if css::selector_matches(&rule.selector, name, id, &classes) {
                decls.overlay(&rule.decls);
            }
        }
        if let Some(inline) = el.attr("style") {
            decls.overlay(&css::parse_decls(inline));
        }
        decls
    }

    /// Render an image: the fetched texture if available, otherwise a
    /// placeholder — and, for remote URLs, a fetch request.
    /// Render an image: an inline `cid:` attachment when available, else a
    /// fetched remote texture, else a placeholder — and, for remote URLs, a
    /// fetch request.
    fn image(&mut self, ui: &mut Ui, el: &scraper::node::Element) {
        self.flush(ui);
        let src = el.attr("src").unwrap_or("").trim();
        let alt = el.attr("alt").unwrap_or("").trim();
        let remote = src.starts_with("http://") || src.starts_with("https://");
        let cid = src
            .strip_prefix("cid:")
            .map(|c| c.trim_matches(['<', '>']).to_string());
        let texture = match cid.as_deref() {
            Some(cid) if !cid.is_empty() => self.ctx.inline_images.get(cid),
            _ => self.ctx.images.get(src).filter(|_| !src.is_empty()),
        };
        match texture {
            Some(texture) => {
                ui.add(egui::Image::from_texture(texture).max_width(ui.available_width().max(1.0)));
            }
            None => {
                if remote {
                    self.ctx.missing_images.push(src.to_string());
                }
                let label = if alt.is_empty() {
                    "[image]".to_string()
                } else {
                    format!("[image: {alt}]")
                };
                ui.label(RichText::new(label).small().weak());
            }
        }
        ui.add_space(4.0);
    }

    fn node(&mut self, ui: &mut Ui, node: NodeRef<'_, Node>, style: &Seg) {
        match node.value() {
            Node::Text(t) => {
                let collapsed = collapse_ws(&t.text);
                self.text(&collapsed, style);
            }
            Node::Element(el) => {
                let name = el.name();
                if SKIPPED.contains(&name) {
                    return;
                }

                // Style resolution: stylesheet rules in document order, then
                // the inline `style` attribute with the highest priority.
                let decls = self.decls_for(el, name);
                if decls.hidden {
                    return;
                }
                let styled = apply_decls(style, &decls, self.dark);
                let style: &Seg = &styled;

                match name {
                    // <style> blocks feed the rule set as the walk reaches
                    // them (head styles precede the body in document order).
                    "style" => {
                        self.rules
                            .extend(css::parse_stylesheet(&collect_pre_text(node)));
                    }
                    "link" => {
                        let is_sheet = el
                            .attr("rel")
                            .is_some_and(|r| r.eq_ignore_ascii_case("stylesheet"));
                        if let (true, Some(href)) = (is_sheet, el.attr("href")) {
                            let href = href.trim();
                            if !self.ctx.stylesheets.contains_key(href) {
                                self.ctx.missing_stylesheets.push(href.to_string());
                            }
                        }
                    }
                    "br" => self.newline(style),
                    "img" => self.image(ui, el),
                    "a" => {
                        let mut s = style.clone();
                        s.link = el
                            .attr("href")
                            .map(str::to_string)
                            .filter(|h| !h.is_empty());
                        s.underline = true;
                        self.children(ui, node, &s);
                    }
                    "b" | "strong" => self.children_with(ui, node, style, |s| s.bold = true),
                    "i" | "em" | "cite" | "var" | "dfn" => {
                        self.children_with(ui, node, style, |s| s.italic = true)
                    }
                    "u" | "ins" => self.children_with(ui, node, style, |s| s.underline = true),
                    "code" | "tt" | "kbd" | "samp" => {
                        self.children_with(ui, node, style, |s| s.mono = true)
                    }
                    "small" => self.children_with(ui, node, style, |s| s.size = Some(12.0)),
                    "big" => self.children_with(ui, node, style, |s| s.size = Some(16.5)),
                    "s" | "strike" | "del" => self.children(ui, node, style), // strikethrough not supported
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        self.flush(ui);
                        let size = match name {
                            "h1" => 22.0,
                            "h2" => 19.0,
                            "h3" => 17.0,
                            "h4" => 15.5,
                            "h5" => 14.5,
                            _ => 13.5,
                        };
                        let heading = with(style, |s| {
                            s.bold = true;
                            s.size = Some(size);
                        });
                        self.children(ui, node, &heading);
                        self.flush(ui);
                        ui.add_space(4.0);
                    }
                    "p" | "div" | "section" | "article" | "header" | "footer" | "main"
                    | "center" | "figure" | "figcaption" | "aside" | "form" | "label"
                    | "fieldset" => {
                        self.flush(ui);
                        self.children(ui, node, style);
                        self.flush(ui);
                        ui.add_space(6.0);
                    }
                    "ul" | "ol" => {
                        self.flush(ui);
                        let ordered = name == "ol";
                        let mut idx: u32 =
                            el.attr("start").and_then(|v| v.parse().ok()).unwrap_or(1);
                        for child in node.children() {
                            if let Node::Element(cel) = child.value() {
                                if cel.name() == "li" {
                                    let marker = if ordered {
                                        format!("{idx}. ")
                                    } else {
                                        "• ".to_string()
                                    };
                                    self.text(&marker, style);
                                    self.children(ui, child, style);
                                    self.flush(ui);
                                    idx += 1;
                                }
                            }
                        }
                        ui.add_space(4.0);
                    }
                    "blockquote" => {
                        self.flush(ui);
                        let id = self.next_id("html-blockquote");
                        let inner = with(style, |s| s.weak = true);
                        ui.indent(id, |ui| {
                            self.children(ui, node, &inner);
                            self.flush(ui);
                        });
                        ui.add_space(6.0);
                    }
                    "pre" => {
                        self.flush(ui);
                        let text = collect_pre_text(node);
                        if !text.trim().is_empty() {
                            let bg = ui.visuals().code_bg_color;
                            ui.label(RichText::new(text).monospace().background_color(bg));
                        }
                        ui.add_space(4.0);
                    }
                    "hr" => {
                        self.flush(ui);
                        ui.separator();
                    }
                    "table" => {
                        self.flush(ui);
                        let rows = table_rows(node);
                        if rows.is_empty() {
                            ui.add_space(4.0);
                        } else if rows.iter().all(|row| row.iter().all(|c| is_flow_only(*c))) {
                            self.render_grid_table(ui, &rows, style);
                        } else {
                            self.render_flow_table(ui, &rows, style);
                        }
                    }
                    _ => self.children(ui, node, style),
                }
            }
            _ => {}
        }
    }

    /// Aligned table for simple tables: every cell contains only inline
    /// content, so egui can size columns sensibly. Cells render inside
    /// `ui::vertical` because grid cells may not call e.g. `add_space`
    /// directly on the grid Ui.
    fn render_grid_table(&mut self, ui: &mut Ui, rows: &[Vec<NodeRef<'_, Node>>], style: &Seg) {
        let id = self.next_id("html-table");
        // Stretch the table over the available width (browsers show HTML
        // mail tables at width="100%"); egui Grid only sizes columns to
        // their content, so give each column at least its even share.
        // Cells with wider content still grow their column.
        let ncols = rows.iter().map(Vec::len).max().unwrap_or(1).max(1) as f32;
        let spacing = ui.spacing().item_spacing.x;
        let available = ui.available_width().max(0.0);
        let share = ((available - spacing * (ncols - 1.0)) / ncols).max(MIN_TABLE_COLUMN_WIDTH);
        egui::Grid::new(id)
            .striped(true)
            .min_col_width(share)
            .show(ui, |ui| {
                for row in rows {
                    for cell in row {
                        let cell_style = with(style, |s| {
                            if matches!(cell.value(), Node::Element(e) if e.name() == "th") {
                                s.bold = true;
                            }
                        });
                        ui.vertical(|ui| {
                            self.children(ui, *cell, &cell_style);
                            self.flush(ui);
                        });
                    }
                    ui.end_row();
                }
            });
        ui.add_space(4.0);
    }

    /// Flowing rows for layout tables (block elements inside cells — the
    /// usual "whole page is one table" HTML mail). egui Grid sizes columns
    /// to the minimum wrapped width, which collapses such content into a
    /// narrow column; instead, cells share one wrapped line separated by
    /// "·" and use the pane's full width.
    fn render_flow_table(&mut self, ui: &mut Ui, rows: &[Vec<NodeRef<'_, Node>>], style: &Seg) {
        let separator = with(style, |s| s.weak = true);
        for row in rows {
            let is_header = row
                .iter()
                .any(|c| matches!(c.value(), Node::Element(e) if e.name() == "th"));
            for (ci, cell) in row.iter().enumerate() {
                if ci > 0 {
                    self.text(" · ", &separator);
                }
                let cell_style = with(style, |s| {
                    if is_header {
                        s.bold = true;
                    }
                });
                self.children(ui, *cell, &cell_style);
            }
            self.flush(ui);
            ui.add_space(2.0);
        }
        ui.add_space(4.0);
    }
}

/// Elements whose entire subtree is dropped (scripts, metadata, form
/// controls, embedded media — none of it is useful or safe to show).
/// `<style>`, `<link>` and `<head>` are NOT skipped: styles apply via
/// `css::parse_stylesheet` and links may reference stylesheets.
const SKIPPED: &[&str] = &[
    "script", "title", "meta", "base", "noscript", "iframe", "template", "input", "select",
    "option", "textarea", "button", "svg", "object", "embed", "canvas", "video", "audio", "map",
    "area",
];

/// Elements rendered as vertical blocks by `Renderer::node`. Keep in sync
/// with the block arms there. Tables containing any of these must render as
/// flowing rows, not as an aligned grid.
const BLOCK_ELEMENTS: &[&str] = &[
    "p",
    "div",
    "section",
    "article",
    "header",
    "footer",
    "main",
    "center",
    "figure",
    "figcaption",
    "aside",
    "form",
    "label",
    "fieldset",
    "address",
    "details",
    "summary",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "ul",
    "ol",
    "li",
    "dl",
    "blockquote",
    "pre",
    "hr",
    "table",
];

/// Readability floor for stretched grid-table columns; tables with more
/// columns than fit keep this minimum and overflow instead of squashing.
const MIN_TABLE_COLUMN_WIDTH: f32 = 60.0;

/// True when a table cell's subtree holds only inline content.
fn is_flow_only(cell: NodeRef<'_, Node>) -> bool {
    cell.descendants()
        .all(|desc| !matches!(desc.value(), Node::Element(e) if BLOCK_ELEMENTS.contains(&e.name())))
}

fn with(style: &Seg, f: impl FnOnce(&mut Seg)) -> Seg {
    let mut s = style.clone();
    f(&mut s);
    s
}

impl Seg {
    fn style_like(&self) -> StyleLike {
        StyleLike {
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
            mono: self.mono,
            weak: self.weak,
            size: self.size,
            color: self.color,
            background: self.background,
            link: self.link.clone(),
        }
    }
}

/// Seg identity for run merging (everything except the text).
#[derive(Clone, PartialEq)]
struct StyleLike {
    bold: bool,
    italic: bool,
    underline: bool,
    mono: bool,
    weak: bool,
    size: Option<f32>,
    color: Option<egui::Color32>,
    background: Option<egui::Color32>,
    link: Option<String>,
}

/// A colour the mail defined, as the theme in force wants it drawn.
///
/// Mail HTML is written for the paper mail is read on — white — and the dark
/// theme draws it on a surface that is nothing like it: the mail's own
/// `#000000` on the card is 1.4:1, which is not text at all. So a colour the
/// mail declares goes through OKLCH — perceptual lightness, chroma, hue —
/// before it is drawn, which is what lets a colour be moved without becoming a
/// different colour:
///
/// * the **light** theme draws every colour the sender wrote, as written: the
///   paper is the paper the mail was written for;
/// * in the dark theme a **background** lighter than 85% lightness becomes the
///   dark surface mail sits on (`0.12, 0.12, 0.14`), so a white-paper mail
///   stops glaring — while one the sender made dark on purpose (a banner, a
///   footer, a dark table head) is already right and is left alone;
/// * **text** darker than 25% lightness becomes near-white
///   (`0.90, 0.90, 0.92`), text lighter than 85% is left alone, and the middle
///   of the range keeps its hue and chroma with its lightness flipped
///   (`1 - l`) — a navy heading comes back a light blue rather than a grey,
///   which is the whole reason for the trip through OKLCH.
///
/// The alpha the mail wrote is kept: the rules above are about lightness, and
/// `rgba(0, 0, 0, 0)` is not a black anybody can read.
fn adapt_color(color: egui::Color32, is_background: bool, dark: bool) -> egui::Color32 {
    use palette::{IntoColor, Oklch, Srgb};

    // The light theme pays nothing for this — not even the conversions.
    if !dark {
        return color;
    }
    let [r, g, b, a] = color.to_srgba_unmultiplied();
    let srgb: Srgb<f32> = Srgb::new(r, g, b).into_format();
    let oklch: Oklch = srgb.into_color();
    let adapted: Srgb<f32> = if is_background {
        if oklch.l > 0.85 {
            Srgb::new(0.12, 0.12, 0.14)
        } else {
            srgb
        }
    } else if oklch.l < 0.25 {
        Srgb::new(0.90, 0.90, 0.92)
    } else if oklch.l > 0.85 {
        srgb
    } else {
        Oklch::new(1.0 - oklch.l, oklch.chroma, oklch.hue).into_color()
    };
    let adapted: Srgb<u8> = adapted.into_format();
    egui::Color32::from_rgba_unmultiplied(adapted.red, adapted.green, adapted.blue, a)
}

fn apply_decls(style: &Seg, decls: &css::Decls, dark: bool) -> Seg {
    let mut s = style.clone();
    if let Some(c) = decls.color {
        s.color = Some(adapt_color(c, false, dark));
    }
    if let Some(b) = decls.background {
        s.background = Some(adapt_color(b, true, dark));
    }
    if let Some(b) = decls.bold {
        s.bold = b;
    }
    if let Some(i) = decls.italic {
        s.italic = i;
    }
    if let Some(u) = decls.underline {
        s.underline = u;
    }
    if let Some(size) = decls.size {
        s.size = Some(size);
    }
    s
}

fn style_rich(seg: &Seg) -> RichText {
    let mut rich = RichText::new(seg.text.as_str());
    if seg.bold {
        rich = rich.strong();
    }
    if seg.italic {
        rich = rich.italics();
    }
    if seg.underline {
        rich = rich.underline();
    }
    if seg.mono {
        rich = rich.monospace();
    }
    if seg.weak {
        rich = rich.weak();
    }
    if let Some(size) = seg.size {
        rich = rich.size(size);
    }
    if let Some(color) = seg.color {
        rich = rich.color(color);
    }
    if let Some(bg) = seg.background {
        rich = rich.background_color(bg);
    }
    rich
}

/// Open a link in the user's browser, but only for schemes we trust. Shared
/// with the Blitz reading-pane renderer, whose links are clickable boxes.
pub(crate) fn open_url(ui: &Ui, url: &str) {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")
    {
        ui.ctx().open_url(egui::OpenUrl::new_tab(url.to_string()));
    }
}

/// Collapse whitespace runs to single spaces, browser-style.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_ws {
                out.push(' ');
                last_ws = true;
            }
        } else {
            out.push(c);
            last_ws = false;
        }
    }
    out
}

/// Raw text of a <pre> block, preserving newlines and skipping tags.
fn collect_pre_text(node: NodeRef<'_, Node>) -> String {
    let mut out = String::new();
    for desc in node.descendants() {
        match desc.value() {
            Node::Text(t) => out.push_str(&t.text),
            Node::Element(e) if e.name() == "br" => out.push('\n'),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// <tr> rows of a table, looking through the implied tbody/thead/tfoot.
fn table_rows(table: NodeRef<'_, Node>) -> Vec<Vec<NodeRef<'_, Node>>> {
    let mut rows = Vec::new();
    for section in table.children() {
        let is_row_container = match section.value() {
            Node::Element(e) => matches!(e.name(), "tbody" | "thead" | "tfoot" | "tr"),
            _ => false,
        };
        if !is_row_container {
            continue;
        }
        if matches!(section.value(), Node::Element(e) if e.name() == "tr") {
            rows.push(
                section
                    .children()
                    .filter(|c| matches!(c.value(), Node::Element(e) if matches!(e.name(), "td" | "th")))
                    .collect(),
            );
        } else {
            for tr in section.children() {
                if matches!(tr.value(), Node::Element(e) if e.name() == "tr") {
                    rows.push(
                        tr.children()
                            .filter(|c| matches!(c.value(), Node::Element(e) if matches!(e.name(), "td" | "th")))
                            .collect(),
                    );
                }
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One frame of a layout test. epaint panics on a dropped
    /// `TexturesDelta` (egui 0.36) and these tests only lay widgets out, so
    /// the frame's texture updates are thrown away here.
    fn run_frame(ctx: &egui::Context, input: egui::RawInput, frame: impl FnMut(&mut egui::Ui)) {
        let mut output = ctx.run_ui(input, frame);
        output.textures_delta.clear();
    }

    /// `parse_fragment` wraps content in an `<html>` element, so tests
    /// search the whole tree for the element they need.
    fn find_element<'a>(frag: &'a Html, name: &str) -> NodeRef<'a, Node> {
        frag.tree
            .root()
            .descendants()
            .find(|n| matches!(n.value(), Node::Element(e) if e.name() == name))
            .unwrap_or_else(|| panic!("{name} element not found"))
    }

    /// An empty HtmlContext for tests that don't care about resources.
    fn empty_ctx<'a>(
        images: &'a HashMap<String, egui::TextureHandle>,
        inline_images: &'a HashMap<String, egui::TextureHandle>,
        stylesheets: &'a HashMap<String, String>,
    ) -> HtmlContext<'a> {
        HtmlContext {
            images,
            inline_images,
            stylesheets,
            missing_images: Vec::new(),
            missing_stylesheets: Vec::new(),
        }
    }

    /// A Renderer over an empty context, for the text-run unit tests that
    /// never touch a Ui.
    fn test_renderer<'x>(hctx: &'x mut HtmlContext<'x>) -> Renderer<'x, 'x> {
        Renderer {
            pending: Vec::new(),
            ids: 0,
            // The light theme: the text-run tests are not about colour.
            dark: false,
            rules: Vec::new(),
            ctx: hctx,
        }
    }

    fn with_empty_ctx<R>(f: impl FnOnce(&mut Renderer<'_, '_>) -> R) -> R {
        let images = HashMap::new();
        let inline = HashMap::new();
        let sheets = HashMap::new();
        let mut hctx = empty_ctx(&images, &inline, &sheets);
        let mut r = test_renderer(&mut hctx);
        f(&mut r)
    }

    #[test]
    fn whitespace_collapses_to_single_spaces() {
        assert_eq!(collapse_ws("a\n  \t b"), "a b");
        assert_eq!(collapse_ws("  leading"), " leading");
    }

    #[test]
    fn equal_style_runs_merge_and_flush_clears() {
        with_empty_ctx(|r| {
            r.text("Hello ", &Seg::default());
            r.text("world", &Seg::default());
            assert_eq!(r.pending.len(), 1);
            assert_eq!(r.pending[0].text, "Hello world");

            let styled = Seg {
                bold: true,
                ..Seg::default()
            };
            r.text("bold", &styled);
            assert_eq!(r.pending.len(), 2);
            r.pending.clear(); // flush() needs a Ui; check reset semantics instead
            r.text("after", &Seg::default());
            assert_eq!(r.pending.len(), 1);
            assert_eq!(r.pending[0].text, "after");
        })
    }

    #[test]
    fn leading_space_is_dropped_at_block_start() {
        with_empty_ctx(|r| {
            r.text("   indented", &Seg::default());
            assert_eq!(r.pending[0].text, "indented");
        })
    }

    #[test]
    fn links_merge_separately_from_plain_text() {
        with_empty_ctx(|r| {
            let linked = Seg {
                link: Some("https://example.com".to_string()),
                ..Seg::default()
            };
            r.text("plain", &Seg::default());
            r.text("link", &linked);
            assert_eq!(r.pending.len(), 2);
        })
    }

    #[test]
    fn table_rows_look_through_implied_tbody() {
        let html = "<table><tbody><tr><td>a</td><td>b</td></tr><tr><td>c</td></tr></tbody></table>";
        let frag = Html::parse_fragment(html);
        let table = find_element(&frag, "table");
        let rows = table_rows(table);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), 2);
        assert_eq!(rows[1].len(), 1);
    }

    #[test]
    fn pre_text_preserves_newlines_and_drops_tags() {
        let html = "<pre>line one\n<b>bold</b>\nline three</pre>";
        let frag = Html::parse_fragment(html);
        let pre = find_element(&frag, "pre");
        assert_eq!(collect_pre_text(pre), "line one\nbold\nline three");
    }

    #[test]
    fn flow_table_detection() {
        let simple = Html::parse_fragment("<table><tr><td>a</td><td>b</td></tr></table>");
        let table = find_element(&simple, "table");
        let rows = table_rows(table);
        assert!(rows.iter().all(|r| r.iter().all(|c| is_flow_only(*c))));

        let layout = Html::parse_fragment("<table><tr><td><p>a</p></td></tr></table>");
        let table = find_element(&layout, "table");
        let rows = table_rows(table);
        assert!(!rows.iter().all(|r| r.iter().all(|c| is_flow_only(*c))));
    }

    #[test]
    fn layout_tables_use_the_available_width() {
        // Regression: layout tables (block elements in cells) rendered via
        // egui Grid collapsed to a ~200px column because the grid sizes
        // columns to the minimum wrapped width. Flowing tables must use
        // (nearly) the full pane width for long content.
        let long_text = "很长的一段正文内容。".repeat(60);
        let html = format!(
            "<html><body><table><tr><td><p>{long_text}</p></td></tr></table></body></html>"
        );
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 800.0),
            )),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        run_frame(&ctx, raw, |ui| {
            let images = HashMap::new();
            let inline = HashMap::new();
            let sheets = HashMap::new();
            let mut hctx = empty_ctx(&images, &inline, &sheets);
            egui::CentralPanel::default().show(ui, |ui| {
                render(ui, &html, &mut hctx);
                assert!(
                    ui.min_rect().width() > 800.0,
                    "flow table should use the pane width, got {}",
                    ui.min_rect().width()
                );
            });
        });
    }

    #[test]
    fn grid_tables_stretch_to_available_width() {
        // Simple inline tables must share the pane width between columns
        // instead of shrinking to content width.
        let html = "<html><body><table><tr><th>k</th><th>v</th></tr>\
                    <tr><td>a</td><td>b</td></tr></table></body></html>";
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 800.0),
            )),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        run_frame(&ctx, raw, |ui| {
            let images = HashMap::new();
            let inline = HashMap::new();
            let sheets = HashMap::new();
            let mut hctx = empty_ctx(&images, &inline, &sheets);
            egui::CentralPanel::default().show(ui, |ui| {
                render(ui, html, &mut hctx);
                assert!(
                    ui.min_rect().width() > 800.0,
                    "grid table should span the pane width, got {}",
                    ui.min_rect().width()
                );
            });
        });
    }

    #[test]
    fn block_elements_inside_table_cells_do_not_panic() {
        // Regression: block elements (p/div/lists/blockquote/hr/pre, nested
        // tables) inside table cells used to panic with "You cannot advance
        // the cursor when in a grid layout". Run the real egui layout to
        // exercise it.
        let ctx = egui::Context::default();
        run_frame(&ctx, egui::RawInput::default(), |ui| {
            let images = HashMap::new();
            let inline = HashMap::new();
            let sheets = HashMap::new();
            let mut hctx = empty_ctx(&images, &inline, &sheets);
            egui::CentralPanel::default().show(ui, |ui| {
                render(
                    ui,
                    "<table>\
                     <tr><th>head</th><td><p>para</p><ul><li>item</li></ul></td></tr>\
                     <tr><td><div>block<div>nested</div></div></td>\
                     <td><table><tr><td><p>inner</p></td></tr></table></td></tr>\
                     <tr><td><blockquote>quote</blockquote><hr></td><td><pre>x\ny</pre></td></tr>\
                     </table>",
                    &mut hctx,
                );
            });
        });
    }

    #[test]
    fn remote_resources_are_collected_for_fetching() {
        let ctx = egui::Context::default();
        run_frame(&ctx, egui::RawInput::default(), |ui| {
            let images = HashMap::new();
            let inline = HashMap::new();
            let sheets = HashMap::new();
            let mut hctx = empty_ctx(&images, &inline, &sheets);
            egui::CentralPanel::default().show(ui, |ui| {
                render(
                    ui,
                    "<link rel=\"stylesheet\" href=\"https://example.com/s.css\">\
                     <img src=\"https://example.com/a.png\" alt=\"pic\">\
                     <img src=\"cid:inline-1\">\
                     <img src=\"data:image/png;base64,AAAA\">",
                    &mut hctx,
                );
            });
            assert_eq!(hctx.missing_images, vec!["https://example.com/a.png"]);
            assert_eq!(hctx.missing_stylesheets, vec!["https://example.com/s.css"]);
        });
    }

    #[test]
    fn cid_images_render_from_the_inline_map() {
        let ctx = egui::Context::default();
        run_frame(&ctx, egui::RawInput::default(), |ui| {
            let images = HashMap::new();
            let sheets = HashMap::new();
            let pixels =
                egui::ColorImage::from_rgba_unmultiplied([2, 2], &[255u8, 0, 0, 255].repeat(4));
            let texture = ctx.load_texture("cid-test", pixels, Default::default());
            let inline = HashMap::from([("logo@example.com".to_string(), texture)]);
            let mut hctx = empty_ctx(&images, &inline, &sheets);
            egui::CentralPanel::default().show(ui, |ui| {
                render(
                    ui,
                    "<p>x <img src=\"cid:logo@example.com\"> y</p>",
                    &mut hctx,
                );
                render(ui, "<img src=\"cid:missing\">", &mut hctx);
            });
            // cid: references never become remote fetches, resolved or not.
            assert!(hctx.missing_images.is_empty());
        });
    }

    #[test]
    fn inline_styles_and_style_blocks_apply_without_panic() {
        let ctx = egui::Context::default();
        run_frame(&ctx, egui::RawInput::default(), |ui| {
            let images = HashMap::new();
            let inline = HashMap::new();
            let sheets = HashMap::new();
            let mut hctx = empty_ctx(&images, &inline, &sheets);
            egui::CentralPanel::default().show(ui, |ui| {
                render(
                    ui,
                    "<head><style>.warn { color: #ff0000; font-weight: bold }</style></head>\
                     <p class=\"warn\" style=\"font-size: 20px\">big red</p>\
                     <p style=\"display: none\">hidden entirely</p>\
                     <p style=\"color: rgb(0, 128, 0); background-color: #ffff00\">green</p>",
                    &mut hctx,
                );
            });
        });
    }

    /// The colours of the text a frame drew, in the order it drew them: the mail
    /// is laid out into a real frame, so what this reads is what would be on
    /// screen rather than what a helper believes would be.
    fn drawn_text_colors(ctx: &egui::Context, html: &str) -> Vec<egui::Color32> {
        let images = HashMap::new();
        let inline = HashMap::new();
        let sheets = HashMap::new();
        let mut hctx = empty_ctx(&images, &inline, &sheets);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                render(ui, html, &mut hctx);
            });
        });
        output.textures_delta.clear();
        output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) => Some(text.galley.job.sections[0].format.color),
                _ => None,
            })
            .collect()
    }

    /// The mail's colours, as each theme draws them. The light theme is the paper
    /// the mail was written for, so every colour is the sender's own; the dark
    /// theme has three rules and this is all of them.
    #[test]
    fn the_dark_theme_adapts_the_colours_the_mail_declares() {
        let black = egui::Color32::from_rgb(0, 0, 0);
        let white = egui::Color32::from_rgb(255, 255, 255);
        // #172B4D: the navy a Word-written mail gives its headings.
        let navy = egui::Color32::from_rgb(23, 43, 77);
        let dark_box = egui::Color32::from_rgb(32, 32, 32);
        let light_text = egui::Color32::from_rgb(230, 232, 236);

        // Light: as written — as text and as a background both.
        for colour in [black, white, navy, dark_box, light_text] {
            assert_eq!(adapt_color(colour, false, false), colour);
            assert_eq!(adapt_color(colour, true, false), colour);
        }

        // Dark, text: black goes near-white, the navy heading comes back a light
        // blue of the same hue, and what is already light stays as it is.
        assert_eq!(
            adapt_color(black, false, true),
            egui::Color32::from_rgb(230, 230, 235)
        );
        assert_eq!(
            adapt_color(navy, false, true),
            egui::Color32::from_rgb(137, 162, 204)
        );
        assert_eq!(adapt_color(light_text, false, true), light_text);

        // Dark, background: white paper becomes the dark surface mail is drawn
        // on, while a box the sender made dark is already right.
        assert_eq!(
            adapt_color(white, true, true),
            egui::Color32::from_rgb(31, 31, 36)
        );
        assert_eq!(adapt_color(dark_box, true, true), dark_box);
    }

    /// The middle of the range keeps what makes it that colour: only lightness is
    /// flipped, so a navy heading is still blue afterwards — a grey would have
    /// been the cheap way to make it readable — and the alpha, which no rule here
    /// is about, comes through untouched.
    #[test]
    fn a_midtone_keeps_its_hue_its_chroma_and_its_alpha() {
        use palette::{IntoColor, Oklch, Srgb};

        let oklch = |colour: egui::Color32| -> Oklch {
            let [r, g, b, _] = colour.to_srgba_unmultiplied();
            let srgb: Srgb<f32> = Srgb::new(r, g, b).into_format();
            srgb.into_color()
        };

        let navy = egui::Color32::from_rgb(23, 43, 77);
        let before = oklch(navy);
        let after = oklch(adapt_color(navy, false, true));
        assert!(
            (after.l - (1.0 - before.l)).abs() < 0.01,
            "lightness is flipped: {} -> {}",
            before.l,
            after.l
        );
        assert!(
            (after.chroma - before.chroma).abs() < 0.02,
            "chroma is kept: {} -> {}",
            before.chroma,
            after.chroma
        );
        assert!(
            (after.hue.into_inner() - before.hue.into_inner()).abs() < 1.0,
            "hue is kept: {} -> {}",
            before.hue.into_inner(),
            after.hue.into_inner()
        );

        // A transparent colour stays transparent: nothing here makes an
        // invisible thing visible, and nothing makes a visible one vanish.
        assert_eq!(adapt_color(egui::Color32::TRANSPARENT, false, true).a(), 0);
        assert_eq!(
            adapt_color(
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 128),
                false,
                true
            )
            .a(),
            128
        );
    }

    /// Which rule a declaration gets is decided by which slot it is: the same
    /// white is text (already light, left alone) and a background (paper, turned
    /// into the dark surface).
    #[test]
    fn a_declaration_is_adapted_as_text_or_as_a_background() {
        let white_text = css::parse_decls("color: #ffffff");
        assert_eq!(
            apply_decls(&Seg::default(), &white_text, true).color,
            Some(egui::Color32::from_rgb(255, 255, 255))
        );

        let white_paper = css::parse_decls("background-color: #ffffff");
        assert_eq!(
            apply_decls(&Seg::default(), &white_paper, true).background,
            Some(egui::Color32::from_rgb(31, 31, 36))
        );

        let black_text = css::parse_decls("color: #000000");
        assert_eq!(
            apply_decls(&Seg::default(), &black_text, true).color,
            Some(egui::Color32::from_rgb(230, 230, 235))
        );
    }

    /// The request, end to end: one mail, two themes. The colour the mail declares
    /// for its text is the sender's in the light theme and the adapted one in the
    /// dark — the theme reaches the walker through the context, not through a
    /// parameter every caller would have to remember to pass.
    #[test]
    fn the_same_mail_is_drawn_in_the_colour_each_theme_wants() {
        let mail = "<p style=\"color: #000000\">paper</p>";

        let light = egui::Context::default();
        crate::theme::install(&light, crate::theme::LIGHT);
        assert_eq!(
            drawn_text_colors(&light, mail),
            vec![egui::Color32::from_rgb(0, 0, 0)]
        );

        let dark = egui::Context::default();
        crate::theme::install(&dark, crate::theme::DARK);
        assert_eq!(
            drawn_text_colors(&dark, mail),
            vec![egui::Color32::from_rgb(230, 230, 235)]
        );
    }
}
