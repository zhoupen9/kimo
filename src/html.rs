//! Renders HTML mail bodies as egui widgets.
//!
//! Parsing is done by html5ever (via `scraper`), so the input is treated
//! exactly like a browser would treat it (error recovery, entity decoding,
//! implied tbody, …). Rendering supports the practical subset of HTML mail:
//! headings, paragraphs, lists, tables, blockquotes, pre/code, inline
//! styling and links. There is deliberately no CSS engine, no JavaScript,
//! and no network access — remote images become placeholders.

use ego_tree::NodeRef;
use egui::{RichText, Ui};
use scraper::{Html, Node};

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
    link: Option<String>,
}

/// Parse `html` and lay it out into `ui` as flowing blocks.
pub fn render(ui: &mut Ui, html: &str) {
    let frag = Html::parse_fragment(html);
    let mut r = Renderer::default();
    for node in frag.tree.root().children() {
        r.node(ui, node, &Seg::default());
    }
    r.flush(ui);
}

#[derive(Default)]
struct Renderer {
    pending: Vec<Seg>,
    ids: u64,
}

impl Renderer {
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
                match name {
                    "br" => self.newline(style),
                    "img" => {
                        let alt = el.attr("alt").unwrap_or("").trim();
                        let label = if alt.is_empty() {
                            "[image]".to_string()
                        } else {
                            format!("[image: {alt}]")
                        };
                        let mut s = style.clone();
                        s.weak = true;
                        self.text(&format!(" {label} "), &s);
                    }
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
                        let id = self.next_id("html-table");
                        egui::Grid::new(id)
                            .striped(true)
                            .show(ui, |ui| {
                                for row in table_rows(node) {
                                    for cell in row {
                                        let cell_style = with(style, |s| {
                                            if matches!(cell.value(), Node::Element(e) if e.name() == "th") {
                                                s.bold = true;
                                            }
                                        });
                                        // Grid cells may only contain widgets;
                                        // cells with block elements (p, div,
                                        // lists…) need a flowing layout, so
                                        // render them inside `ui::vertical`
                                        // (calling e.g. `add_space` on the
                                        // grid-cell Ui itself panics).
                                        ui.vertical(|ui| {
                                            self.children(ui, cell, &cell_style);
                                            self.flush(ui);
                                        });
                                    }
                                    ui.end_row();
                                }
                            });
                        ui.add_space(4.0);
                    }
                    _ => self.children(ui, node, style),
                }
            }
            _ => {}
        }
    }
}

/// Elements whose entire subtree is dropped (scripts, metadata, form
/// controls, embedded media — none of it is useful or safe to show).
const SKIPPED: &[&str] = &[
    "script", "style", "head", "title", "meta", "link", "base", "noscript", "iframe", "template",
    "input", "select", "option", "textarea", "button", "svg", "object", "embed", "canvas", "video",
    "audio", "map", "area",
];

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
    link: Option<String>,
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
    rich
}

fn open_url(ui: &Ui, url: &str) {
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

    /// `parse_fragment` wraps content in an `<html>` element, so tests
    /// search the whole tree for the element they need.
    fn find_element<'a>(frag: &'a Html, name: &str) -> NodeRef<'a, Node> {
        frag.tree
            .root()
            .descendants()
            .find(|n| matches!(n.value(), Node::Element(e) if e.name() == name))
            .unwrap_or_else(|| panic!("{name} element not found"))
    }

    #[test]
    fn whitespace_collapses_to_single_spaces() {
        assert_eq!(collapse_ws("a\n  \t b"), "a b");
        assert_eq!(collapse_ws("  leading"), " leading");
    }

    #[test]
    fn equal_style_runs_merge_and_flush_clears() {
        let mut r = Renderer::default();
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
    }

    #[test]
    fn leading_space_is_dropped_at_block_start() {
        let mut r = Renderer::default();
        r.text("   indented", &Seg::default());
        assert_eq!(r.pending[0].text, "indented");
    }

    #[test]
    fn links_merge_separately_from_plain_text() {
        let linked = Seg {
            link: Some("https://example.com".to_string()),
            ..Seg::default()
        };
        let mut r = Renderer::default();
        r.text("plain", &Seg::default());
        r.text("link", &linked);
        assert_eq!(r.pending.len(), 2);
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
    fn block_elements_inside_table_cells_do_not_panic() {
        // Regression: block elements (p/div/lists/blockquote/hr/pre, nested
        // tables) inside table cells used to panic with "You cannot advance
        // the cursor when in a grid layout". Run the real egui layout to
        // exercise it.
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                render(
                    ui,
                    "<table>\
                     <tr><th>head</th><td><p>para</p><ul><li>item</li></ul></td></tr>\
                     <tr><td><div>block<div>nested</div></div></td>\
                     <td><table><tr><td><p>inner</p></td></tr></table></td></tr>\
                     <tr><td><blockquote>quote</blockquote><hr></td><td><pre>x\ny</pre></td></tr>\
                     </table>",
                );
            });
        });
    }
}
