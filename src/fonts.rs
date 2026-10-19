//! System font discovery and egui font installation.
//!
//! egui cannot enumerate fonts, and the project may not use fontconfig (hard
//! rule 1), so the installed families are found by walking the usual font
//! directories and reading the few tables a face's identity needs (`name`,
//! `OS/2`, `post`) rather than the font file itself: a desktop's CJK
//! collections alone are gigabytes, and reading them left the allocator
//! holding tens of megabytes after every scan.
//!
//! The scan still runs as a worker job (`sync::Job::ScanFonts`) that comes
//! back as `Event::FontsScanned`, because a directory walk is filesystem work;
//! only the one-file read needed to install a picked family happens on the UI
//! thread.
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
use std::io::{Read, Seek, SeekFrom};
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

// ----- reading a face without reading its file -------------------------------

/// What one face declares about itself: what the family picker and Blitz's
/// font registration ask for, and nothing else.
///
/// These fields live in the face's `name`, `OS/2` and `post` tables — a few
/// KiB altogether — so they are read table by table instead of handing the
/// whole file to `ttf-parser`, whose `Face` borrows the entire font. A font
/// directory can hold gigabytes of CJK collections; scanning it must not read
/// them.
///
/// A face whose tables cannot be read still takes its slot in [`faces`], with
/// no family, so the face indexes stored in `settings.json` keep pointing at
/// the face they meant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FaceInfo {
    /// Typographic family (name ID 16) when the font has one, otherwise the
    /// legacy family (ID 1): "DejaVu Sans", not "DejaVu Sans Bold".
    pub family: Option<String>,
    /// Typographic subfamily (ID 17) when present, otherwise the legacy one
    /// (ID 2): "Regular", "Bold", "SemiBold", "Bold Italic".
    pub subfamily: Option<String>,
    /// `OS/2` `usWeightClass`; 400 when the table is missing, as `ttf-parser`
    /// reports it.
    pub weight: u16,
    /// `OS/2` fsSelection bit 5.
    pub bold: bool,
    /// `OS/2` fsSelection bit 0, or a non-zero `post` italic angle.
    pub italic: bool,
    /// `post` `isFixedPitch`.
    pub monospace: bool,
}

/// A `name` table is normally a few KiB; this only stops a corrupt length from
/// making the scan allocate a huge buffer.
const MAX_NAME_TABLE: u32 = 1 << 20;
/// OS/2 version 5 is 100 bytes and is the longest; the fields read here end at
/// offset 64.
const MAX_OS2_TABLE: u32 = 100;
/// `post` is parsed from its first 32 bytes.
const MAX_POST_TABLE: u32 = 32;
/// No desktop font collection comes near this many faces.
const MAX_FACES: usize = 1024;

/// A font file open for the few reads a scan makes.
struct FontFile {
    file: std::fs::File,
    len: u64,
}

impl FontFile {
    fn open(path: &Path) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        Some(Self { file, len })
    }

    /// `len` bytes at `offset`, or `None` when that runs past the end of the
    /// file — a font claiming a table it does not carry.
    fn read(&mut self, offset: u64, len: u32) -> Option<Vec<u8>> {
        if offset.checked_add(u64::from(len))? > self.len {
            return None;
        }
        let mut buffer = vec![0; len as usize];
        self.file.seek(SeekFrom::Start(offset)).ok()?;
        self.file.read_exact(&mut buffer).ok()?;
        Some(buffer)
    }
}

/// Where each face starts: one zero for a plain TTF/OTF, or the `ttcf`
/// collection's face offsets. A collection's header is tag, version, count,
/// then one offset per face.
fn face_offsets(font: &mut FontFile) -> Option<Vec<u64>> {
    let header = font.read(0, 12)?;
    if &header[0..4] != b"ttcf" {
        return Some(vec![0]);
    }
    let count = u32_at(&header, 8)? as usize;
    if count == 0 || count > MAX_FACES {
        return None;
    }
    let offsets = font.read(12, (count * 4) as u32)?;
    Some(
        offsets
            .as_chunks::<4>()
            .0
            .iter()
            .map(|offset| u64::from(u32::from_be_bytes(*offset)))
            .collect(),
    )
}

/// The ranges of the three tables a scan reads, as `(offset, length)`.
#[derive(Default)]
struct Tables {
    name: Option<(u64, u32)>,
    os2: Option<(u64, u32)>,
    post: Option<(u64, u32)>,
}

/// The tables of the face whose table directory starts at `offset`.
///
/// The version is checked against the four sfnt flavours (`TrueType`, `OTTO`,
/// `true`, `typ1`) so a collection offset pointing into the middle of a table
/// is not mistaken for a face.
fn face_tables(font: &mut FontFile, offset: u64) -> Option<Tables> {
    let header = font.read(offset, 12)?;
    let version = u32_at(&header, 0)?;
    if !matches!(
        version,
        0x0001_0000 | 0x4F54_544F | 0x7472_7565 | 0x7479_7031
    ) {
        return None;
    }
    let count = u16_at(&header, 4)? as usize;
    if count == 0 || count > MAX_FACES {
        return None;
    }
    let directory = font.read(offset + 12, (count * 16) as u32)?;
    let mut tables = Tables::default();
    for record in directory.as_chunks::<16>().0 {
        let slot = if &record[0..4] == b"name" {
            &mut tables.name
        } else if &record[0..4] == b"OS/2" {
            &mut tables.os2
        } else if &record[0..4] == b"post" {
            &mut tables.post
        } else {
            continue;
        };
        if slot.is_none() {
            *slot = Some((u64::from(u32_at(record, 8)?), u32_at(record, 12)?));
        }
    }
    Some(tables)
}

/// One face's name, style and fixed-pitch flag, read from its three tables.
fn face_info(font: &mut FontFile, offset: u64) -> FaceInfo {
    let Some(tables) = face_tables(font, offset) else {
        return FaceInfo::default();
    };
    let (family, subfamily) = tables
        .name
        .and_then(|(offset, len)| font.read(offset, len.min(MAX_NAME_TABLE)))
        .map(|data| parse_names(&data))
        .unwrap_or((None, None));
    let os2 = tables
        .os2
        .and_then(|(offset, len)| font.read(offset, len.min(MAX_OS2_TABLE)))
        .and_then(|data| parse_os2(&data));
    let post = tables
        .post
        .and_then(|(offset, len)| font.read(offset, len.min(MAX_POST_TABLE)))
        .and_then(|data| parse_post(&data));
    FaceInfo {
        family,
        subfamily,
        weight: os2.as_ref().map_or(400, |os2| os2.weight),
        bold: os2
            .as_ref()
            .is_some_and(|os2| os2.fs_selection & (1 << 5) != 0),
        italic: os2.as_ref().is_some_and(|os2| os2.fs_selection & 1 != 0)
            || post.as_ref().is_some_and(|post| post.italic_angle != 0),
        monospace: post.as_ref().is_some_and(|post| post.monospaced),
    }
}

/// Every face of the font at `path`, in collection order.
///
/// A face that cannot be read is still returned, with no family, so an index
/// into the list keeps naming the same face. An unreadable file yields an
/// empty list.
pub fn faces(path: &Path) -> Vec<FaceInfo> {
    let Some(mut font) = FontFile::open(path) else {
        return Vec::new();
    };
    let Some(offsets) = face_offsets(&mut font) else {
        return Vec::new();
    };
    offsets
        .into_iter()
        .map(|offset| face_info(&mut font, offset))
        .collect()
}

fn u16_at(data: &[u8], offset: usize) -> Option<u16> {
    let bytes = data.get(offset..offset + 2)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// A face's family and subfamily, both preferring the typographic records
/// (name IDs 16/17) over the legacy ones (1/2).
///
/// Only the Unicode platform and the Windows symbol/BMP encodings are decoded,
/// which is what `ttf-parser` reads: a Macintosh record is skipped rather than
/// guessed at, and the names the picker stores do not change between this
/// reader and a full parse.
fn parse_names(data: &[u8]) -> (Option<String>, Option<String>) {
    let count = u16_at(data, 2).map_or(0, usize::from);
    let Some(storage) = u16_at(data, 4).and_then(|offset| data.get(usize::from(offset)..)) else {
        return (None, None);
    };
    let mut family = None;
    let mut typographic_family = None;
    let mut subfamily = None;
    let mut typographic_subfamily = None;
    for index in 0..count {
        let start = 6 + index * 12;
        let Some(record) = data.get(start..start + 12) else {
            break;
        };
        let name_id = u16_at(record, 6).unwrap_or(0);
        let length = usize::from(u16_at(record, 8).unwrap_or(0));
        let offset = usize::from(u16_at(record, 10).unwrap_or(0));
        let Some(end) = offset.checked_add(length) else {
            continue;
        };
        let Some(bytes) = storage.get(offset..end) else {
            continue;
        };
        let platform = u16_at(record, 0).unwrap_or(0);
        let encoding = u16_at(record, 2).unwrap_or(0);
        let Some(text) = decode_name(platform, encoding, bytes) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        match name_id {
            ttf_parser::name_id::TYPOGRAPHIC_FAMILY if typographic_family.is_none() => {
                typographic_family = Some(text);
            }
            ttf_parser::name_id::FAMILY if family.is_none() => family = Some(text),
            ttf_parser::name_id::TYPOGRAPHIC_SUBFAMILY if typographic_subfamily.is_none() => {
                typographic_subfamily = Some(text);
            }
            ttf_parser::name_id::SUBFAMILY if subfamily.is_none() => subfamily = Some(text),
            _ => {}
        }
    }
    (
        typographic_family.or(family),
        typographic_subfamily.or(subfamily),
    )
}

/// Decode a name record in an encoding `ttf-parser` also reads: the Unicode
/// platform, or Windows' symbol and Unicode-BMP encodings (UTF-16BE either
/// way). Other platforms are Mac Roman records, which it skips.
fn decode_name(platform: u16, encoding: u16, bytes: &[u8]) -> Option<String> {
    let unicode = platform == 0 || (platform == 3 && encoding <= 1);
    if !unicode {
        return None;
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes(*pair))
        .collect();
    String::from_utf16(&units).ok()
}

/// The `OS/2` fields the rank needs, when the table is a version this reader
/// knows — the same versions `ttf-parser` accepts, with its length checks.
struct Os2 {
    weight: u16,
    fs_selection: u16,
}

fn parse_os2(data: &[u8]) -> Option<Os2> {
    let expected = match u16_at(data, 0)? {
        0 => 78,
        1 => 86,
        2..=4 => 96,
        5 => 100,
        _ => return None,
    };
    if data.len() < expected {
        return None;
    }
    Some(Os2 {
        weight: u16_at(data, 4)?,
        // usWeightClass, then fsSelection after the panose block and the
        // Unicode ranges.
        fs_selection: u16_at(data, 62)?,
    })
}

/// The `post` fields a scan needs. The table must be at least 32 bytes and a
/// version `ttf-parser` accepts, or it is ignored altogether.
struct Post {
    italic_angle: i32,
    monospaced: bool,
}

fn parse_post(data: &[u8]) -> Option<Post> {
    if data.len() < 32 {
        return None;
    }
    let version = u32_at(data, 0)?;
    if !matches!(
        version,
        0x0001_0000 | 0x0002_0000 | 0x0002_5000 | 0x0003_0000 | 0x0004_0000
    ) {
        return None;
    }
    let angle: [u8; 4] = data.get(4..8)?.try_into().ok()?;
    Some(Post {
        italic_angle: i32::from_be_bytes(angle),
        monospaced: u32_at(data, 12)? != 0,
    })
}

/// All installed families by name, sorted by name. Reads each face's `name`,
/// `OS/2` and `post` tables rather than the font file — call it from a worker
/// thread (`Job::ScanFonts`), never per frame.
pub fn scan() -> Vec<FontFamily> {
    // The rank keeps a family's bold or italic face from shadowing its regular
    // one, whichever the directory walk reaches first.
    let mut families: BTreeMap<String, (u16, FontFamily)> = BTreeMap::new();
    for path in font_files() {
        for (index, face) in faces(&path).into_iter().enumerate() {
            let Some(name) = face.family.as_deref() else {
                continue;
            };
            let candidate = (
                face_rank(&face),
                FontFamily {
                    name: name.to_string(),
                    path: path.clone(),
                    index: index as u32,
                    monospace: face.monospace,
                },
            );
            match families.get(name) {
                Some((rank, _)) if *rank <= candidate.0 => {}
                _ => {
                    families.insert(name.to_string(), candidate);
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
fn face_rank(face: &FaceInfo) -> u16 {
    let bold = if face.bold { 1000 } else { 0 };
    let italic = if face.italic { 500 } else { 0 };
    bold + italic + face.weight.abs_diff(400)
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

    // ----- the name-only reader ---------------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `bytes` written to a path unique to this test process.
    fn temp_font(bytes: &[u8]) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "kimo-fonts-{}-{}.ttf",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, bytes).expect("write the test font");
        path
    }

    /// An sfnt whose table directory starts at `base`, carrying `tables` in
    /// order the way a real font lays them out.
    fn sfnt_at(base: usize, tables: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let data_start = base + 12 + 16 * tables.len();
        let mut body = Vec::new();
        let mut records = Vec::new();
        for (tag, table) in tables {
            records.push((*tag, data_start + body.len(), table.len()));
            body.extend_from_slice(table);
            while body.len() % 4 != 0 {
                body.push(0);
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        out.extend_from_slice(&(tables.len() as u16).to_be_bytes());
        out.extend_from_slice(&[0; 6]);
        for (tag, offset, len) in records {
            out.extend_from_slice(tag);
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&(offset as u32).to_be_bytes());
            out.extend_from_slice(&(len as u32).to_be_bytes());
        }
        out.extend_from_slice(&body);
        out
    }

    fn sfnt(tables: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        sfnt_at(0, tables)
    }

    /// A `.ttc` holding one face per entry, each at its own file offset.
    fn ttc(faces: &[&[(&[u8; 4], Vec<u8>)]]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"ttcf");
        out.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        out.extend_from_slice(&(faces.len() as u32).to_be_bytes());
        let mut offset = 12 + 4 * faces.len();
        let mut built = Vec::new();
        for tables in faces {
            out.extend_from_slice(&(offset as u32).to_be_bytes());
            let face = sfnt_at(offset, tables);
            offset += face.len();
            built.push(face);
        }
        for face in built {
            out.extend_from_slice(&face);
        }
        out
    }

    /// A format-0 `name` table for `(platform, encoding, name ID, text)`
    /// records, stored as UTF-16BE like the fonts Windows/Unicode records are.
    fn name_table(records: &[(u16, u16, u16, &str)]) -> Vec<u8> {
        let mut strings = Vec::new();
        let mut entries = Vec::new();
        for (platform, encoding, name_id, text) in records {
            let encoded: Vec<u8> = text
                .encode_utf16()
                .flat_map(|unit| unit.to_be_bytes())
                .collect();
            entries.push((
                *platform,
                *encoding,
                *name_id,
                encoded.len() as u16,
                strings.len() as u16,
            ));
            strings.extend_from_slice(&encoded);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_be_bytes());
        out.extend_from_slice(&(6 + 12 * entries.len() as u16).to_be_bytes());
        for (platform, encoding, name_id, len, offset) in entries {
            out.extend_from_slice(&platform.to_be_bytes());
            out.extend_from_slice(&encoding.to_be_bytes());
            out.extend_from_slice(&0u16.to_be_bytes());
            out.extend_from_slice(&name_id.to_be_bytes());
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&offset.to_be_bytes());
        }
        out.extend_from_slice(&strings);
        out
    }

    /// A version-0 `OS/2` table with these weight and fsSelection values.
    fn os2_table(weight: u16, fs_selection: u16) -> Vec<u8> {
        let mut out = vec![0u8; 78];
        out[4..6].copy_from_slice(&weight.to_be_bytes());
        out[62..64].copy_from_slice(&fs_selection.to_be_bytes());
        out
    }

    /// A version-2 `post` table with this italic angle and fixed-pitch flag.
    fn post_table(italic_angle: f32, monospaced: bool) -> Vec<u8> {
        let mut out = vec![0u8; 32];
        out[0..4].copy_from_slice(&0x0002_0000u32.to_be_bytes());
        out[4..8].copy_from_slice(&((italic_angle * 65536.0) as i32).to_be_bytes());
        out[12..16].copy_from_slice(&u32::from(monospaced).to_be_bytes());
        out
    }

    #[test]
    fn a_face_is_read_from_its_name_os2_and_post_tables() {
        let path = temp_font(&sfnt(&[
            (
                b"name",
                name_table(&[
                    (3, 1, 1, "DejaVu Sans"),
                    (3, 1, 2, "Bold"),
                    (3, 1, 16, "DejaVu Sans"),
                    (3, 1, 17, "SemiBold"),
                ]),
            ),
            (b"OS/2", os2_table(600, 1 << 5)),
            (b"post", post_table(0.0, true)),
        ]));
        let faces = faces(&path);
        assert_eq!(faces.len(), 1);
        assert_eq!(faces[0].family.as_deref(), Some("DejaVu Sans"));
        // The typographic subfamily (ID 17) beats the legacy one (ID 2).
        assert_eq!(faces[0].subfamily.as_deref(), Some("SemiBold"));
        assert_eq!(faces[0].weight, 600);
        assert!(faces[0].bold);
        assert!(!faces[0].italic);
        assert!(faces[0].monospace);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn a_post_italic_angle_marks_the_face_italic() {
        let path = temp_font(&sfnt(&[
            (b"name", name_table(&[(3, 1, 1, "Georgia")])),
            (b"OS/2", os2_table(400, 0)),
            (b"post", post_table(-12.0, false)),
        ]));
        let faces = faces(&path);
        assert_eq!(faces[0].family.as_deref(), Some("Georgia"));
        assert!(faces[0].italic);
        assert!(!faces[0].bold);
        assert!(!faces[0].monospace);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn mac_only_names_are_skipped_like_ttf_parser_skips_them() {
        let path = temp_font(&sfnt(&[(b"name", name_table(&[(1, 0, 1, "Helvetica")]))]));
        let faces = faces(&path);
        assert_eq!(faces.len(), 1);
        assert_eq!(faces[0].family, None);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn collection_faces_keep_their_slots_when_one_cannot_be_read() {
        let named: [(&[u8; 4], Vec<u8>); 1] =
            [(b"name", name_table(&[(3, 1, 1, "Noto Sans CJK SC")]))];
        let unnamed: [(&[u8; 4], Vec<u8>); 1] = [(b"post", post_table(0.0, false))];
        let path = temp_font(&ttc(&[named.as_slice(), unnamed.as_slice()]));
        let faces = faces(&path);
        assert_eq!(faces.len(), 2);
        assert_eq!(faces[0].family.as_deref(), Some("Noto Sans CJK SC"));
        // The second face still occupies index 1, so a stored face index keeps
        // naming the face it named when it was stored.
        assert_eq!(faces[1].family, None);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn a_truncated_table_reads_as_an_unknown_face() {
        let mut bytes = sfnt(&[(b"name", name_table(&[(3, 1, 1, "Truncated Sans")]))]);
        bytes.truncate(12 + 16);
        let path = temp_font(&bytes);
        let faces = faces(&path);
        assert_eq!(faces.len(), 1);
        assert_eq!(faces[0].family, None);
        std::fs::remove_file(path).ok();
    }

    /// The picker's names and the face it stores are what `settings.json`
    /// keeps, so the name-only reader must say exactly what a full
    /// `ttf-parser` parse says. Runs on whatever fonts the machine has.
    #[test]
    fn the_name_only_reader_agrees_with_ttf_parser() {
        let mut compared = 0;
        for path in font_files().into_iter().take(24) {
            let Ok(data) = std::fs::read(&path) else {
                continue;
            };
            for (index, ours) in faces(&path).iter().enumerate() {
                let Ok(face) = ttf_parser::Face::parse(&data, index as u32) else {
                    continue;
                };
                let where_ = format!("{} face {index}", path.display());
                assert_eq!(ours.family, face_family_name(&face), "{where_}");
                assert_eq!(ours.weight, face.weight().to_number(), "{where_}");
                assert_eq!(ours.bold, face.is_bold(), "{where_}");
                assert_eq!(ours.italic, face.is_italic(), "{where_}");
                assert_eq!(ours.monospace, face.is_monospaced(), "{where_}");
                compared += 1;
            }
        }
        assert!(
            compared > 0 || font_files().is_empty(),
            "no installed face was comparable"
        );
    }
}
