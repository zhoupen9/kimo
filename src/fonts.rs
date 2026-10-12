//! System font discovery and egui font installation.
//!
//! egui cannot enumerate fonts, and the project may not use fontconfig (hard
//! rule 1), so the installed families are found by walking the usual font
//! directories and reading each face's `name` table with `ttf-parser` — pure
//! Rust, and already compiled in through Blitz (fontique/fontdb).
//!
//! Reading every installed face takes a noticeable moment on a machine with
//! hundreds of fonts, so it is a worker job (`sync::Job::ScanFonts`) that
//! comes back as `Event::FontsScanned`; only the one-file read needed to
//! install a picked family happens on the UI thread.
//!
//! The choice made in Preferences → Appearance is applied to the egui UI (and
//! therefore to the built-in HTML renderer, `crate::html`). Blitz keeps its
//! own curated font set — see `blitz_render::load_fonts`.
//!
//! The CJK fallback is a pan-CJK collection (Noto Sans CJK), whose faces share
//! one glyph repertoire but not the forms they pick from it: the face has to
//! follow the reader's language, or Chinese mail comes out with Japanese glyph
//! variants. See `CjkScript`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Font directories worth scanning, in preference order. Missing ones are
/// skipped, so this needs no configuration on any Unix.
pub const FONT_DIRS: &[&str] = &[
    "/usr/share/fonts",
    "/usr/local/share/fonts",
    "/usr/share/X11/fonts",
    "/System/Library/Fonts",
    "/Library/Fonts",
    "/Library/Fonts/Microsoft",
];

const FONT_EXTENSIONS: &[&str] = &["ttf", "otf", "ttc", "otc"];

/// How far below each root to look; fonts live at most a few levels down
/// (`fonts/<vendor>/<family>/<face>.ttf`).
const SCAN_DEPTH: usize = 4;

/// One installed font family, represented by its plainest face.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontFamily {
    pub name: String,
    pub path: PathBuf,
    /// Face inside `path`; `.ttc` collections hold several.
    pub index: u32,
    /// The face declares itself fixed-pitch (its `post` table says so). Used
    /// as a hint in the pickers, not a filter: some monospaced families —
    /// variable ones especially — leave the flag unset.
    pub monospace: bool,
}

/// Every font file under the standard directories, sorted and deduplicated.
/// Lists paths only, so it is cheap enough to call from anywhere.
pub fn font_files() -> Vec<PathBuf> {
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

    let mut files = Vec::new();
    for root in roots {
        collect_fonts(&root, SCAN_DEPTH, &mut files);
    }
    files.sort();
    files.dedup();
    files
}

/// All installed families by name, sorted by name. Reads every face's name
/// table — call it from a worker thread (`Job::ScanFonts`), never per frame.
pub fn scan() -> Vec<FontFamily> {
    // The rank keeps a family's bold or italic face from shadowing its regular
    // one, whichever the directory walk reaches first.
    let mut families: BTreeMap<String, (u16, FontFamily)> = BTreeMap::new();
    for path in font_files() {
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let faces = ttf_parser::fonts_in_collection(&data).unwrap_or(1);
        for index in 0..faces {
            let Ok(face) = ttf_parser::Face::parse(&data, index) else {
                continue;
            };
            let Some(name) = face_family_name(&face) else {
                continue;
            };
            let candidate = (
                face_rank(&face),
                FontFamily {
                    name: name.clone(),
                    path: path.clone(),
                    index,
                    monospace: face.is_monospaced(),
                },
            );
            match families.get(&name) {
                Some((rank, _)) if *rank <= candidate.0 => {}
                _ => {
                    families.insert(name, candidate);
                }
            }
        }
    }
    families.into_values().map(|(_, family)| family).collect()
}

/// Look a family up by the name remembered in `settings.json`. Comparison is
/// case-insensitive so a hand-edited file still resolves.
pub fn find<'a>(families: &'a [FontFamily], name: &str) -> Option<&'a FontFamily> {
    families
        .iter()
        .find(|family| family.name.eq_ignore_ascii_case(name))
}

/// Install the egui fonts: the picked families first (so they win for Latin
/// text), the CJK fallback last (egui's built-ins only cover Latin/Cyrillic/
/// Greek, and mail needs more).
///
/// Also binds the named families `kimo_ui`/`kimo_mono` mirroring both slots,
/// so the Preferences page can render previews in the picked fonts — epaint
/// panics on a named family that is not bound to any font, so they always
/// exist even when nothing is picked.
pub fn install(ctx: &egui::Context, ui: Option<&FontFamily>, mono: Option<&FontFamily>) {
    let mut fonts = egui::FontDefinitions::default();

    if let Some((bytes, index)) = cjk_font() {
        fonts
            .font_data
            .insert(CJK_KEY.into(), font_data(bytes, index));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push(CJK_KEY.into());
        }
    }

    for (slot, picked) in [
        (egui::FontFamily::Proportional, ui),
        (egui::FontFamily::Monospace, mono),
    ] {
        let Some(picked) = picked else {
            continue;
        };
        let Ok(bytes) = std::fs::read(&picked.path) else {
            continue;
        };
        let key = format!("kimo_font_{}", fonts.font_data.len());
        fonts
            .font_data
            .insert(key.clone(), font_data(bytes, picked.index));
        fonts.families.entry(slot).or_default().insert(0, key);
    }

    for (name, slot) in [
        ("kimo_ui", egui::FontFamily::Proportional),
        ("kimo_mono", egui::FontFamily::Monospace),
    ] {
        let list = fonts.families.entry(slot).or_default().clone();
        fonts
            .families
            .insert(egui::FontFamily::Name(name.into()), list);
    }

    ctx.set_fonts(fonts);
}

const CJK_KEY: &str = "kimo_cjk";

/// Best-effort CJK fallback, in preference order.
const CJK_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-sans-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJKsc-Regular.otf",
    "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
    "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
    "/usr/share/fonts/wenquanyi/wqy-zenhei/wqy-zenhei.ttc",
];

/// Which CJK glyph variant to fall back to.
///
/// Noto Sans CJK (and Source Han Sans) ship one file per weight holding the
/// Simplified, Traditional, Japanese and Korean faces. They cover the same
/// characters, but each face picks the *forms* its language writes: the
/// Japanese face draws 复 the Japanese way, which is what Chinese mail looked
/// like while this module took the collection's first face — the Japanese one.
/// So the face follows the locale, the same environment fontconfig reads, and
/// for the same reason.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CjkScript {
    Simplified,
    Traditional,
    Japanese,
    Korean,
}

impl CjkScript {
    /// The tag the faces carry in their family name ("Noto Sans CJK SC").
    fn tag(self) -> &'static str {
        match self {
            CjkScript::Simplified => "SC",
            CjkScript::Traditional => "TC",
            CjkScript::Japanese => "JP",
            CjkScript::Korean => "KR",
        }
    }

    /// The script the reader's locale asks for. Anything unrecognised — an
    /// unset locale, `C`, a Latin one — gets Simplified Chinese: it is this
    /// project's default CJK audience (the folder-name table in `folders.rs`
    /// is Chinese too), and the other variants stay readable to everyone else.
    fn from_locale() -> Self {
        for key in ["LC_ALL", "LC_CTYPE", "LANG", "LANGUAGE"] {
            let Ok(value) = std::env::var(key) else {
                continue;
            };
            // LANGUAGE is a colon-separated preference list.
            if let Some(first) = value.split(':').find(|entry| !entry.is_empty()) {
                return script_of_locale(first);
            }
        }
        CjkScript::Simplified
    }
}

/// The CJK variant a locale name asks for: `zh_TW.UTF-8` → Traditional,
/// `ja_JP` → Japanese, `zh_CN`/`en_US`/`C` → Simplified.
fn script_of_locale(locale: &str) -> CjkScript {
    let name = locale.split(['.', '@']).next().unwrap_or_default();
    let mut parts = name.split(['_', '-']);
    let language = parts.next().unwrap_or_default().to_ascii_lowercase();
    let territory = parts.next().unwrap_or_default().to_ascii_lowercase();
    match language.as_str() {
        "ja" => CjkScript::Japanese,
        "ko" => CjkScript::Korean,
        "zh" if matches!(territory.as_str(), "tw" | "hk" | "mo") => CjkScript::Traditional,
        _ => CjkScript::Simplified,
    }
}

/// The face to use for `script`, given a collection's family names in face
/// order. The `Mono` faces are skipped: they are for monospace contexts, and
/// spending a second copy of a ~20 MB face on the rare CJK `<pre>` is not
/// worth it. `None` means the file carries no face for that script — a
/// single-language font has exactly one face, which the caller takes as it is.
fn pick_cjk_face(names: &[Option<String>], script: CjkScript) -> Option<u32> {
    names
        .iter()
        .position(|name| {
            name.as_deref()
                .is_some_and(|name| !name.contains("Mono") && name.ends_with(script.tag()))
        })
        .map(|index| index as u32)
}

/// The first CJK font that is installed, with the face the locale asks for.
fn cjk_font() -> Option<(Vec<u8>, u32)> {
    let script = CjkScript::from_locale();
    CJK_CANDIDATES.iter().find_map(|path| {
        let bytes = std::fs::read(path).ok()?;
        let faces = ttf_parser::fonts_in_collection(&bytes).unwrap_or(1);
        // One slot per face, unreadable ones included: dropping them would
        // shift the indexes the faces are chosen by.
        let names: Vec<Option<String>> = (0..faces)
            .map(|index| {
                ttf_parser::Face::parse(&bytes, index)
                    .ok()
                    .and_then(|face| face_family_name(&face))
            })
            .collect();
        let index = pick_cjk_face(&names, script).unwrap_or(0);
        Some((bytes, index))
    })
}

fn font_data(bytes: Vec<u8>, index: u32) -> std::sync::Arc<egui::FontData> {
    let mut data = egui::FontData::from_owned(bytes);
    data.index = index;
    data.into()
}

/// The family name of a face: the typographic family (name ID 16) when the
/// font has one, otherwise the legacy family (ID 1). The legacy entry repeats
/// the style on many fonts ("DejaVu Sans Bold"), which would split one family
/// into an entry per weight.
fn face_family_name(face: &ttf_parser::Face<'_>) -> Option<String> {
    use ttf_parser::name_id;

    let mut legacy = None;
    for name in face.names() {
        match name.name_id {
            name_id::TYPOGRAPHIC_FAMILY => {
                if let Some(text) = name.to_string().filter(|text| !text.is_empty()) {
                    return Some(text);
                }
            }
            name_id::FAMILY if legacy.is_none() => legacy = name.to_string(),
            _ => {}
        }
    }
    legacy.filter(|name| !name.is_empty())
}

/// Lower is plainer: the face a UI would pick by default. Regular beats bold,
/// italic and off weights — plenty of families ship a lighter face before
/// their Regular one (DejaVu Sans arrives as ExtraLight), and the picker
/// should not load that one just because the directory walk saw it first.
fn face_rank(face: &ttf_parser::Face<'_>) -> u16 {
    let bold = if face.is_bold() { 1000 } else { 0 };
    let italic = if face.is_italic() { 500 } else { 0 };
    bold + italic + face.weight().to_number().abs_diff(400)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn family(name: &str) -> FontFamily {
        FontFamily {
            name: name.to_string(),
            path: PathBuf::from("/nonexistent/font.ttf"),
            index: 0,
            monospace: false,
        }
    }

    fn names(list: &[&str]) -> Vec<Option<String>> {
        list.iter().map(|name| Some(name.to_string())).collect()
    }

    #[test]
    fn the_locale_selects_the_cjk_variant() {
        assert_eq!(script_of_locale("zh_CN.UTF-8"), CjkScript::Simplified);
        assert_eq!(script_of_locale("zh-Hans-CN"), CjkScript::Simplified);
        assert_eq!(script_of_locale("zh_TW.Big5"), CjkScript::Traditional);
        assert_eq!(script_of_locale("zh-HK"), CjkScript::Traditional);
        assert_eq!(script_of_locale("ja_JP.utf8"), CjkScript::Japanese);
        assert_eq!(script_of_locale("ko_KR"), CjkScript::Korean);
        assert_eq!(script_of_locale("en_US.UTF-8"), CjkScript::Simplified);
        assert_eq!(script_of_locale("C"), CjkScript::Simplified);
        assert_eq!(script_of_locale(""), CjkScript::Simplified);
    }

    #[test]
    fn the_cjk_face_follows_the_script_not_the_face_order() {
        // The order NotoSansCJK-Regular.ttc really ships in. Taking the first
        // face — which is what this used to do — hands Chinese mail Japanese
        // glyphs.
        let faces = names(&[
            "Noto Sans CJK JP",
            "Noto Sans CJK KR",
            "Noto Sans CJK SC",
            "Noto Sans CJK TC",
            "Noto Sans CJK HK",
            "Noto Sans Mono CJK JP",
            "Noto Sans Mono CJK KR",
            "Noto Sans Mono CJK SC",
            "Noto Sans Mono CJK TC",
            "Noto Sans Mono CJK HK",
        ]);
        assert_eq!(pick_cjk_face(&faces, CjkScript::Simplified), Some(2));
        assert_eq!(pick_cjk_face(&faces, CjkScript::Traditional), Some(3));
        assert_eq!(pick_cjk_face(&faces, CjkScript::Japanese), Some(0));
        assert_eq!(pick_cjk_face(&faces, CjkScript::Korean), Some(1));
        // Monospace faces never win the proportional slot.
        let mono_only = names(&["Noto Sans Mono CJK SC"]);
        assert_eq!(pick_cjk_face(&mono_only, CjkScript::Simplified), None);
        // A single-language face is used whichever tag it carries, and a
        // script the file does not have leaves the choice to the caller.
        let simplified_only = names(&["Noto Sans CJK SC"]);
        assert_eq!(
            pick_cjk_face(&simplified_only, CjkScript::Simplified),
            Some(0)
        );
        assert_eq!(pick_cjk_face(&simplified_only, CjkScript::Japanese), None);
        // A face that failed to parse must not shift the ones after it.
        let gappy = vec![None, Some("Noto Sans CJK SC".to_string())];
        assert_eq!(pick_cjk_face(&gappy, CjkScript::Simplified), Some(1));
    }

    #[test]
    fn find_matches_family_names_case_insensitively() {
        let families = vec![family("DejaVu Sans"), family("DejaVu Sans Mono")];
        assert_eq!(find(&families, "DejaVu Sans").unwrap().name, "DejaVu Sans");
        assert_eq!(find(&families, "dejavu sans").unwrap().name, "DejaVu Sans");
        assert_eq!(
            find(&families, "DEJAVU SANS MONO").unwrap().name,
            "DejaVu Sans Mono"
        );
        // Prefixes and unknown names must not match.
        assert!(find(&families, "DejaVu").is_none());
        assert!(find(&families, "Noto Sans").is_none());
        assert!(find(&[], "DejaVu Sans").is_none());
    }
}
