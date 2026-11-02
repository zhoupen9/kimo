//! A deliberately tiny CSS subset for HTML mail: a fixed set of properties
//! (colors, font weight/style/size, text-decoration, background,
//! display/visibility) with stylesheets using single-part tag/class/id
//! selectors. This is not a CSS engine — compound selectors, cascading
//! rules, units beyond the basics and everything layout-related are simply
//! ignored. Remote stylesheets are fetched by sync workers
//! (`Job::FetchStylesheet`) and injected here for parsing.

use egui::Color32;

/// The subset of declarations we understand, already resolved to values.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Decls {
    pub color: Option<Color32>,
    pub background: Option<Color32>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub size: Option<f32>,
    pub hidden: bool,
}

impl Decls {
    /// Overlay `other` on top of `self` (later declarations win; `None`
    /// keeps the earlier value).
    pub fn overlay(&mut self, other: &Decls) {
        if other.color.is_some() {
            self.color = other.color;
        }
        if other.background.is_some() {
            self.background = other.background;
        }
        if other.bold.is_some() {
            self.bold = other.bold;
        }
        if other.italic.is_some() {
            self.italic = other.italic;
        }
        if other.underline.is_some() {
            self.underline = other.underline;
        }
        if other.size.is_some() {
            self.size = other.size;
        }
        if other.hidden {
            self.hidden = true;
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Selector {
    Tag(String),
    Class(String),
    Id(String),
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub selector: Selector,
    pub decls: Decls,
}

pub fn selector_matches(sel: &Selector, tag: &str, id: Option<&str>, classes: &[&str]) -> bool {
    match sel {
        Selector::Tag(want) => want == tag,
        Selector::Id(want) => id == Some(want.as_str()),
        Selector::Class(want) => classes.contains(&want.as_str()),
    }
}

/// Parse `prop: value; prop: value; …` for the supported properties;
/// unknown properties are ignored.
pub fn parse_decls(text: &str) -> Decls {
    let mut d = Decls::default();
    for part in text.split(';') {
        let Some((key, value)) = part.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "color" => d.color = parse_color(value),
            "background-color" => d.background = parse_color(value),
            "background" => {
                // The shorthand may be several layered values — `white
                // url(a.png) no-repeat` — of which this subset keeps the
                // colour. The whole value is tried first, because a colour may
                // have spaces of its own (`rgb(255, 255, 255)` is one value
                // and not three tokens), and the first token is the fallback
                // for the rest.
                d.background = parse_color(value)
                    .or_else(|| value.split_whitespace().next().and_then(parse_color));
            }
            "font-weight" => {
                d.bold = match value.to_ascii_lowercase().as_str() {
                    "bold" | "bolder" => Some(true),
                    "normal" | "lighter" => Some(false),
                    number => number.parse::<u16>().ok().map(|n| n >= 600),
                };
            }
            "font-style" => {
                d.italic = match value.to_ascii_lowercase().as_str() {
                    "italic" | "oblique" => Some(true),
                    "normal" => Some(false),
                    _ => None,
                };
            }
            "text-decoration" | "text-decoration-line" => {
                let value = value.to_ascii_lowercase();
                d.underline = Some(value.contains("underline"));
            }
            "font-size" => d.size = parse_size(value),
            "display" if value.eq_ignore_ascii_case("none") => d.hidden = true,
            "visibility" if value.eq_ignore_ascii_case("hidden") => d.hidden = true,
            _ => {}
        }
    }
    d
}

/// Parse a stylesheet into rules. Comments are stripped; selector lists are
/// split on commas; only single-part selectors (tag / `.class` / `#id`)
/// are kept, everything else is ignored.
pub fn parse_stylesheet(text: &str) -> Vec<Rule> {
    let text = strip_comments(text);
    let mut rules = Vec::new();
    for block in text.split('}') {
        let Some((selectors, body)) = block.split_once('{') else {
            continue;
        };
        let decls = parse_decls(body);
        if decls == Decls::default() {
            continue;
        }
        for selector in selectors.split(',') {
            let selector = selector.trim();
            let parsed = if let Some(class) = selector.strip_prefix('.') {
                is_identifier(class).then(|| Selector::Class(class.to_string()))
            } else if let Some(id) = selector.strip_prefix('#') {
                is_identifier(id).then(|| Selector::Id(id.to_string()))
            } else if is_identifier(selector) {
                Some(Selector::Tag(selector.to_ascii_lowercase()))
            } else {
                None
            };
            if let Some(selector) = parsed {
                rules.push(Rule { selector, decls });
            }
        }
    }
    rules
}

fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Font size: px, pt (×4/3), em/rem (×14), bare numbers, and the common
/// absolute-size keywords.
pub fn parse_size(value: &str) -> Option<f32> {
    let value = value.trim().to_ascii_lowercase();
    let keyword = match value.as_str() {
        "xx-small" => Some(9.0),
        "x-small" => Some(10.5),
        "small" => Some(12.0),
        "medium" => Some(14.0),
        "large" => Some(16.5),
        "x-large" => Some(20.0),
        "xx-large" => Some(24.0),
        _ => None,
    };
    if let Some(px) = keyword {
        return Some(px);
    }
    let (number, unit) = value.split_at(value.find(|c: char| !c.is_ascii_digit() && c != '.')?);
    let number: f32 = number.trim().parse().ok()?;
    let unit = unit.trim();
    match unit {
        "px" => Some(number),
        "pt" => Some(number * 4.0 / 3.0),
        "em" | "rem" => Some(number * 14.0),
        "%" => Some(number * 14.0 / 100.0),
        "" => Some(number), // unitless ≈ px
        _ => None,
    }
}

/// A CSS colour as egui wants it, read by `csscolorparser`: the named colours,
/// `#rgb`/`#rgba`/`#rrggbb`/`#rrggbbaa`, `rgb()`/`rgba()`/`hsl()`/`hsla()` and the
/// rest of CSS Colour 4. Mail is written by Word and Outlook as much as by
/// hand, and those spell a colour `rgb(23, 43, 77)` as readily as `#172B4D` —
/// the hand-rolled table this replaced read the second and dropped the first.
/// The alpha a colour carries comes with it (`Color32` is where it lives).
/// `None` for anything that is not a colour, and for the parts of a shorthand
/// that are not one.
pub fn parse_color(value: &str) -> Option<Color32> {
    let parsed = csscolorparser::parse(value.trim()).ok()?;
    let [r, g, b, a] = parsed.to_rgba8();
    Some(Color32::from_rgba_unmultiplied(r, g, b, a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_parse_the_supported_subset() {
        let d = parse_decls("color: #FF0000; font-weight: bold; font-size: 12px; display: none");
        assert_eq!(d.color, Some(Color32::from_rgb(255, 0, 0)));
        assert_eq!(d.bold, Some(true));
        assert_eq!(d.size, Some(12.0));
        assert!(d.hidden);

        let d = parse_decls("font-weight: normal; text-decoration: underline");
        assert_eq!(d.bold, Some(false));
        assert_eq!(d.underline, Some(true));

        // Unknown properties are ignored entirely.
        let d = parse_decls("margin: 4px; position: absolute");
        assert_eq!(d, Decls::default());
    }

    #[test]
    fn font_weight_numbers_map_at_600() {
        assert_eq!(parse_decls("font-weight: 700").bold, Some(true));
        assert_eq!(parse_decls("font-weight: 400").bold, Some(false));
    }

    #[test]
    fn sizes_parse_with_units() {
        assert_eq!(parse_size("12px"), Some(12.0));
        assert_eq!(parse_size("9pt"), Some(12.0));
        assert_eq!(parse_size("1.5em"), Some(21.0));
        assert_eq!(parse_size("large"), Some(16.5));
        assert_eq!(parse_size("10cm"), None);
    }

    #[test]
    fn colors_parse_named_hex_and_short() {
        // The named colours are CSS's own values, not shades of Kimo's: what
        // the mail asks for is what the mail gets.
        assert_eq!(parse_color("RED"), Some(Color32::from_rgb(255, 0, 0)));
        assert_eq!(parse_color("#abc"), Some(Color32::from_rgb(170, 187, 204)));
        assert_eq!(
            parse_color("#aabbcc"),
            Some(Color32::from_rgb(170, 187, 204))
        );
        assert_eq!(
            parse_color("#abcd"),
            Some(Color32::from_rgba_unmultiplied(170, 187, 204, 221))
        );
        assert_eq!(parse_color("notacolor"), None);
    }

    /// What the parser reads beyond hex and the names: the functions mail
    /// actually uses, the alpha they carry, and the shorthand's own shape —
    /// a colour with spaces in it is one value, not three tokens.
    #[test]
    fn colors_parse_functions_alpha_and_shorthand() {
        assert_eq!(
            parse_color("rgb(23, 43, 77)"),
            Some(Color32::from_rgb(23, 43, 77))
        );
        assert_eq!(
            parse_color("hsl(0, 100%, 50%)"),
            Some(Color32::from_rgb(255, 0, 0))
        );
        assert_eq!(
            parse_color("transparent"),
            Some(Color32::from_rgba_unmultiplied(0, 0, 0, 0))
        );
        assert_eq!(
            parse_color("rebeccapurple"),
            Some(Color32::from_rgb(102, 51, 153))
        );
        // A length, a URL and nonsense are not colours.
        assert_eq!(parse_color("12px"), None);
        assert_eq!(parse_color("url(a.png)"), None);

        let white = Some(Color32::from_rgb(255, 255, 255));
        assert_eq!(
            parse_decls("background-color: rgb(255, 255, 255)").background,
            white
        );
        assert_eq!(
            parse_decls("background: rgb(255, 255, 255)").background,
            white
        );
        assert_eq!(
            parse_decls("background: white url(a.png) no-repeat").background,
            white
        );
    }

    #[test]
    fn stylesheets_parse_selectors_and_strip_comments() {
        let rules = parse_stylesheet(
            "/* comment */ .notice { color: red; } p, #main { font-weight: bold; }\n\
             div.x > span { color: blue; } @media (max-width: 100px) { p { color: lime } }",
        );
        let selectors: Vec<&Selector> = rules.iter().map(|r| &r.selector).collect();
        assert!(selectors.contains(&&Selector::Class("notice".into())));
        assert!(selectors.contains(&&Selector::Tag("p".into())));
        assert!(selectors.contains(&&Selector::Id("main".into())));
        // Compound selectors and at-rules are ignored.
        assert_eq!(rules.len(), 3);
        assert!(rules[0].decls.color.is_some());
    }

    #[test]
    fn selector_matching_by_tag_class_id() {
        let classes = ["notice", "big"];
        assert!(selector_matches(
            &Selector::Tag("p".into()),
            "p",
            None,
            &classes
        ));
        assert!(selector_matches(
            &Selector::Class("notice".into()),
            "p",
            None,
            &classes
        ));
        assert!(selector_matches(
            &Selector::Id("main".into()),
            "p",
            Some("main"),
            &classes
        ));
        assert!(!selector_matches(
            &Selector::Id("other".into()),
            "p",
            Some("main"),
            &classes
        ));
    }

    #[test]
    fn overlay_later_wins() {
        let mut base = parse_decls("color: red; font-weight: bold");
        base.overlay(&parse_decls("color: blue"));
        // CSS's blue, which is what a mail asking for `blue` means.
        assert_eq!(base.color, Some(Color32::from_rgb(0, 0, 255)));
        assert_eq!(base.bold, Some(true));
    }
}
