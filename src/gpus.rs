//! The machine's graphics cards, and the Vulkan drivers that may load for them.
//!
//! The Vulkan loader opens every installed ICD (the `*_icd.json` manifests) when
//! an instance is created, so a machine with two cards pays for both driver
//! stacks — on this box the NVIDIA stack and Mesa's Intel + lavapipe — although
//! only one of them can draw Kimo's window. Preferences → Graphics turns that
//! into the reader's choice: [`startup_plan`] reads the saved card and
//! [`apply`] hands the loader that card's manifests alone (`VK_DRIVER_FILES`,
//! and the `VK_ICD_FILENAMES` name an older loader knows), before
//! `crate::gpu` makes its instance. The other cards' libraries are then never
//! opened — which is the point: an adapter preference would only choose between
//! drivers that had already been loaded.
//!
//! Three rules keep a bad preference from leaving the app without a GPU:
//!
//! * A pick is honoured only while the card is in the machine. A slot that is
//!   not there — an eGPU on another desk, a `settings.json` carried from another
//!   machine, a typo — is ignored and every driver loads, exactly as before the
//!   preference existed.
//! * A manifest is dropped only when its library name says whose driver it is
//!   (`libvulkan_nvidia.so`, `libvulkan_intel.so`, …). A driver this table does
//!   not know is *kept*: not being able to attribute it is no reason to take
//!   the machine's only renderer away.
//! * A restriction that would leave no driver at all is not applied.
//!
//! Software rasterizers (lavapipe, SwiftShader) are dropped when a card is
//! picked: they serve no card, and lavapipe is the one ICD that pulls Mesa's
//! LLVM in by itself. A session that picked nothing keeps loading them.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;

/// The display-controller base class (`0x03xxxx`) of the PCI specification.
const DISPLAY_CLASS: u32 = 0x03;

/// Kernel drivers whose DRM card cannot render: a framebuffer the firmware
/// left behind, a virtual test device, and the Virtual GEM helper. They are
/// not offered as GPUs.
const NON_RENDERING_DRIVERS: [&str; 3] = ["simpledrm", "vkms", "vgem"];

/// Where `pci.ids` may be, in the order they are tried. Only the few lines
/// naming this machine's cards are kept, so the file's size does not reach the
/// app.
const PCI_IDS: [&str; 4] = [
    "/usr/share/hwdata/pci.ids",
    "/usr/share/misc/pci.ids",
    "/usr/share/pci.ids",
    "/usr/local/share/hwdata/pci.ids",
];

/// Library names a vendor's Vulkan driver goes by, as substrings, mapped to
/// the PCI vendor ID the cards in sysfs carry.
///
/// This is a name table on purpose: the ICD manifest format has no vendor
/// field (only the library's path), and reading a driver's ELF to find one
/// would be a different kind of bet. Unknown names are kept (see [`plan`]), so
/// the table only has to be right about the drivers that are *dropped*.
const VENDOR_LIBRARIES: [(&str, u32); 16] = [
    ("intel", 0x8086),
    ("nvidia", 0x10de),
    ("radeon", 0x1002),
    ("amdvlk", 0x1002),
    ("amdgpu", 0x1002),
    ("freedreno", 0x5143),
    ("turnip", 0x5143),
    ("panfrost", 0x13b5),
    ("panvk", 0x13b5),
    ("mali", 0x13b5),
    ("powervr", 0x1010),
    ("asahi", 0x106b),
    ("virtio", 0x1af4),
    ("venus", 0x1af4),
    ("broadcom", 0x14e4),
    ("v3dv", 0x14e4),
];

/// Library names of the software rasterizers, which serve no card at all.
const SOFTWARE_LIBRARIES: [&str; 6] = [
    "lvp",
    "lavapipe",
    "swiftshader",
    "llvmpipe",
    "softpipe",
    "swrast",
];

/// One card the kernel has a rendering driver for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    /// The PCI address, e.g. `0000:03:00.0` — what the kernel, the Vulkan
    /// driver and `settings.json` all know the card by.
    pub pci: String,
    /// The PCI vendor ID (`0x8086` for Intel).
    pub vendor: u32,
    /// The PCI device ID (`0xe20b` for this box's Arc B580).
    pub device: u32,
    /// The kernel driver bound to it: `xe`, `nvidia`, `amdgpu`, …
    pub driver: String,
}

impl Card {
    /// The card one `/sys/class/drm/cardN/device/uevent` describes, when it is
    /// a PCI display controller with a driver that can render.
    fn from_uevent(text: &str) -> Option<Self> {
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name))
                .map(str::trim)
        };
        let driver = field("DRIVER=")?;
        if NON_RENDERING_DRIVERS.contains(&driver) {
            return None;
        }
        let class = u32::from_str_radix(field("PCI_CLASS=")?, 16).ok()?;
        if class >> 16 != DISPLAY_CLASS {
            return None;
        }
        let (vendor, device) = field("PCI_ID=")?.split_once(':')?;
        Some(Self {
            vendor: u32::from_str_radix(vendor, 16).ok()?,
            device: u32::from_str_radix(device, 16).ok()?,
            pci: field("PCI_SLOT_NAME=")?.to_ascii_lowercase(),
            driver: driver.to_string(),
        })
    }

    /// The model's name, as `pci.ids` gives it.
    pub fn title(&self, names: &Names) -> String {
        names
            .device(self.vendor, self.device)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{} GPU", vendor_name(self.vendor).unwrap_or("Unknown")))
    }

    /// The vendor, the PCI address and the kernel driver — the line under the
    /// name, where two cards of one vendor are told apart.
    pub fn subtitle(&self, names: &Names) -> String {
        let vendor = names
            .vendor(self.vendor)
            .or_else(|| vendor_name(self.vendor))
            .unwrap_or("Unknown vendor");
        format!("{vendor} · {} · {}", self.pci, self.driver)
    }
}

/// The short vendor names the cards are introduced by when `pci.ids` is not
/// installed; the database's fuller names ("Intel Corporation") win when it is.
fn vendor_name(vendor: u32) -> Option<&'static str> {
    match vendor {
        0x8086 => Some("Intel"),
        0x10de => Some("NVIDIA"),
        0x1002 => Some("AMD"),
        0x13b5 => Some("ARM"),
        0x5143 => Some("Qualcomm"),
        0x1010 => Some("Imagination"),
        0x106b => Some("Apple"),
        0x1af4 => Some("Red Hat"),
        0x14e4 => Some("Broadcom"),
        _ => None,
    }
}

/// The `pci.ids` names of the cards a scan found — only those, so a 1.7 MB
/// database never has to live in the app.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Names {
    vendors: HashMap<u32, String>,
    devices: HashMap<(u32, u32), String>,
}

impl Names {
    pub fn vendor(&self, vendor: u32) -> Option<&str> {
        self.vendors.get(&vendor).map(String::as_str)
    }

    pub fn device(&self, vendor: u32, device: u32) -> Option<&str> {
        self.devices.get(&(vendor, device)).map(String::as_str)
    }
}

/// The `pci.ids` lines that name `cards`, parsed out of `text`.
///
/// The file's shape is four hex digits and a name at the left margin for a
/// vendor, one tab and the same for a device, and a `C 03` heading for the
/// class list — which resets the vendor, or every class name would be read as
/// a device of the vendor above it.
pub fn names_from(text: &str, cards: &[Card]) -> Names {
    let wanted_vendors: HashSet<u32> = cards.iter().map(|card| card.vendor).collect();
    let wanted_devices: HashSet<(u32, u32)> = cards
        .iter()
        .map(|card| (card.vendor, card.device))
        .collect();
    let mut names = Names::default();
    let mut vendor = None;
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        if !line.starts_with('\t') {
            // A vendor's line, or a class heading. `C 03` is not four hex
            // digits, so it clears the vendor and names nothing.
            vendor = line
                .get(..4)
                .and_then(|id| u32::from_str_radix(id, 16).ok());
            if let Some((id, name)) = hex_name(line, 4) {
                if wanted_vendors.contains(&id) {
                    names.vendors.entry(id).or_insert(name);
                }
            }
            continue;
        }
        let rest = &line[1..];
        if rest.starts_with('\t') {
            // A subsystem: two tabs, and no name the picker wants.
            continue;
        }
        let Some(vendor) = vendor else { continue };
        if let Some((id, name)) = hex_name(rest, 4) {
            if wanted_devices.contains(&(vendor, id)) {
                names.devices.entry((vendor, id)).or_insert(name);
            }
        }
    }
    names
}

/// The `id  name` shape of a `pci.ids` line: `digits` hex digits, then two
/// spaces, then the name.
fn hex_name(line: &str, digits: usize) -> Option<(u32, String)> {
    let id = line.get(..digits)?;
    if !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let rest = line.get(digits..)?;
    if !rest.starts_with(' ') {
        return None;
    }
    let name = rest.trim();
    if name.is_empty() {
        return None;
    }
    Some((u32::from_str_radix(id, 16).ok()?, name.to_string()))
}

/// The names for `cards` from the first `pci.ids` that has them.
fn names_for(cards: &[Card]) -> Names {
    let mut names = Names::default();
    if cards.is_empty() {
        return names;
    }
    for path in PCI_IDS {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let found = names_from(&text, cards);
        for (vendor, name) in found.vendors {
            names.vendors.entry(vendor).or_insert(name);
        }
        for (key, name) in found.devices {
            names.devices.entry(key).or_insert(name);
        }
        if cards
            .iter()
            .all(|card| names.device(card.vendor, card.device).is_some())
        {
            break;
        }
    }
    names
}

/// One installed Vulkan driver: its manifest, the library it names, and, when
/// the library's name says so, the card vendor it drives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Icd {
    pub path: PathBuf,
    /// What the manifest spells as `library_path`: a file name or an absolute
    /// path.
    pub library: String,
    /// The vendor whose cards this driver serves, from the library's name.
    pub vendor: Option<u32>,
    /// A software rasterizer, which serves no card at all.
    pub software: bool,
}

/// The `ICD` object of a manifest, and nothing else it carries.
#[derive(Deserialize)]
struct Manifest {
    #[serde(rename = "ICD")]
    icd: Option<ManifestEntry>,
}

#[derive(Deserialize)]
struct ManifestEntry {
    library_path: Option<String>,
}

/// The manifest at `path`, read from `text`. A file that is not a manifest, or
/// names no library, is not a driver Kimo can say anything about.
fn parse_manifest(path: &Path, text: &str) -> Option<Icd> {
    let manifest: Manifest = serde_json::from_str(text).ok()?;
    let library = manifest.icd?.library_path?;
    let software = is_software(&library);
    Some(Icd {
        path: path.to_path_buf(),
        vendor: if software { None } else { vendor_of(&library) },
        library,
        software,
    })
}

/// The vendor a driver library belongs to, by the name it is installed under.
fn vendor_of(library: &str) -> Option<u32> {
    let name = library
        .rsplit('/')
        .next()
        .unwrap_or(library)
        .to_ascii_lowercase();
    VENDOR_LIBRARIES
        .iter()
        .find_map(|(needle, vendor)| name.contains(needle).then_some(*vendor))
}

/// Whether the library is a software rasterizer rather than a card's driver.
fn is_software(library: &str) -> bool {
    let name = library
        .rsplit('/')
        .next()
        .unwrap_or(library)
        .to_ascii_lowercase();
    SOFTWARE_LIBRARIES
        .iter()
        .any(|needle| name.contains(needle))
}

/// The directories the Vulkan loader reads ICD manifests from, in its own
/// order: the per-user directory, the local prefixes, `/etc`, then each
/// `XDG_DATA_DIRS` entry.
fn icd_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    if let Some(data_home) = data_home {
        dirs.push(data_home.join("vulkan/icd.d"));
    }
    dirs.push(PathBuf::from("/usr/local/etc/vulkan/icd.d"));
    dirs.push(PathBuf::from("/usr/local/share/vulkan/icd.d"));
    dirs.push(PathBuf::from("/etc/vulkan/icd.d"));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for dir in data_dirs.split(':').filter(|dir| !dir.is_empty()) {
        dirs.push(Path::new(dir).join("vulkan/icd.d"));
    }
    dirs
}

/// Every ICD manifest installed for this session, sorted by path so the log
/// and the page list them the same way twice.
pub fn icds() -> Vec<Icd> {
    let mut found = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for dir in icd_dirs() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            // The same manifest can be reachable through two directories.
            let key = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if !seen.insert(key) {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            if let Some(icd) = parse_manifest(&path, &text) {
                found.push(icd);
            }
        }
    }
    found.sort_by(|one, other| one.path.cmp(&other.path));
    found
}

/// The DRM nodes under sysfs. Each `cardN` is one card; `cardN-DP-1` and the
/// like are its connectors and are not read.
const DRM_DIR: &str = "/sys/class/drm";

/// The cards this machine has a rendering driver for, sorted by PCI address.
pub fn cards() -> Vec<Card> {
    let mut found: Vec<Card> = Vec::new();
    let Ok(entries) = fs::read_dir(DRM_DIR) else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(number) = name.strip_prefix("card") else {
            continue;
        };
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let uevent = entry.path().join("device/uevent");
        let Ok(text) = fs::read_to_string(&uevent) else {
            continue;
        };
        let Some(card) = Card::from_uevent(&text) else {
            continue;
        };
        if !found.iter().any(|other| other.pci == card.pci) {
            found.push(card);
        }
    }
    found.sort_by(|one, other| one.pci.cmp(&other.pci));
    found
}

/// Whether `slot` has the shape of a PCI address (`0000:03:00.0`). Only the
/// shape: whether that card is installed is [`plan`]'s question.
fn is_pci_slot(slot: &str) -> bool {
    let bytes = slot.as_bytes();
    let hex = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_hexdigit);
    bytes.len() == 12
        && hex(0..4)
        && bytes[4] == b':'
        && hex(5..7)
        && bytes[7] == b':'
        && hex(8..10)
        && bytes[10] == b'.'
        && (b'0'..=b'7').contains(&bytes[11])
}

/// Whether two PCI addresses name the same card. The domain is dropped from
/// both: Vulkan's `VkPhysicalDevicePCIBusInfo` may leave it off, while sysfs
/// always writes it.
pub fn same_slot(one: &str, other: &str) -> bool {
    fn key(slot: &str) -> String {
        let slot = slot.trim().to_ascii_lowercase();
        let parts: Vec<&str> = slot.split(':').collect();
        if parts.len() == 3 {
            parts[1..].join(":")
        } else {
            slot
        }
    }
    key(one) == key(other)
}

/// What the saved preference came to, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// No card picked: every installed driver loads, as before.
    Automatic,
    /// The picked card. Its drivers alone load when that drops anything —
    /// [`Plan::restriction`] is the test.
    Picked(Card),
    /// The setting names a card that is not installed.
    NotPresent(String),
    /// The setting is no PCI address at all.
    Malformed(String),
    /// The card is here, but no installed driver serves it.
    NoDriver(Card),
    /// The session already names the drivers, so the setting is not applied.
    Environment,
}

/// What start-up does with the Vulkan loader's driver list.
#[derive(Clone, Debug)]
pub struct Plan {
    /// The value `settings.json` held, for the log and the page.
    pub saved: Option<String>,
    pub status: Status,
    /// The manifests the loader is given — all of them when nothing is
    /// restricted.
    pub drivers: Vec<PathBuf>,
    /// Every manifest the scan found, so a caller can say what was left out.
    pub all: Vec<PathBuf>,
}

impl Plan {
    /// The manifests to hand the loader, or `None` when its own list stands.
    /// A pick that drops nothing is not a restriction: overriding the loader
    /// to give it the list it would have built anyway only risks losing a
    /// manifest this module did not find.
    pub fn restriction(&self) -> Option<&[PathBuf]> {
        match self.status {
            Status::Picked(_) if self.drivers.len() < self.all.len() => Some(&self.drivers),
            _ => None,
        }
    }

    /// The card this process should draw with, when a pick was honoured.
    pub fn card(&self) -> Option<&Card> {
        match &self.status {
            Status::Picked(card) => Some(card),
            _ => None,
        }
    }
}

/// What to do with the driver list, from the saved value and what is on the
/// machine. Pure: the I/O is [`cards`], [`icds`] and [`environment_lock`], so
/// every fallback below is a test.
pub fn plan(saved: Option<&str>, cards: &[Card], icds: &[Icd], locked: bool) -> Plan {
    let all: Vec<PathBuf> = icds.iter().map(|icd| icd.path.clone()).collect();
    let fallback = |status: Status| Plan {
        saved: saved.map(str::to_string),
        status,
        drivers: all.clone(),
        all: all.clone(),
    };
    let Some(saved) = saved else {
        return fallback(Status::Automatic);
    };
    if locked {
        return fallback(Status::Environment);
    }
    if !is_pci_slot(saved) {
        return fallback(Status::Malformed(saved.to_string()));
    }
    let Some(card) = cards
        .iter()
        .find(|card| card.pci.eq_ignore_ascii_case(saved))
    else {
        return fallback(Status::NotPresent(saved.to_string()));
    };
    // What may serve this card: its vendor's drivers, and the ones nothing is
    // known about. Measured against `all`, a list that drops something is a
    // restriction; one that does not is not worth an override.
    let drivers: Vec<PathBuf> = icds
        .iter()
        .filter(|icd| !icd.software && icd.vendor.is_none_or(|vendor| vendor == card.vendor))
        .map(|icd| icd.path.clone())
        .collect();
    if drivers.is_empty() {
        return fallback(Status::NoDriver(card.clone()));
    }
    Plan {
        saved: Some(saved.to_string()),
        status: Status::Picked(card.clone()),
        drivers,
        all,
    }
}

/// The list this process set itself, so the page does not mistake Kimo's own
/// `VK_DRIVER_FILES` for the session's.
static OWN_DRIVERS: OnceLock<String> = OnceLock::new();

/// This process's plan, decided once. `main` applies it before the event loop
/// exists and `crate::gpu` reads the card back out of it for its adapter.
static PLAN: OnceLock<Plan> = OnceLock::new();

pub fn startup_plan() -> &'static Plan {
    PLAN.get_or_init(|| {
        let saved = crate::storage::load_settings().gpu;
        plan(saved.as_deref(), &cards(), &icds(), environment_lock())
    })
}

/// Start-up's one call: decide, and tell the loader.
pub fn choose_drivers() {
    apply(startup_plan());
}

/// Whether the session already names the Vulkan drivers itself — by
/// `VK_DRIVER_FILES`, its old `VK_ICD_FILENAMES` name, or one of the loader's
/// newer selection knobs. Kimo's own `VK_DRIVER_FILES` does not count: the
/// value it set is remembered in [`OWN_DRIVERS`].
pub fn environment_lock() -> bool {
    let own = OWN_DRIVERS.get().map(String::as_str);
    for var in ["VK_DRIVER_FILES", "VK_ICD_FILENAMES"] {
        if let Some(value) = std::env::var_os(var) {
            if own != Some(value.to_string_lossy().as_ref()) {
                return true;
            }
        }
    }
    std::env::var_os("VK_ADD_DRIVER_FILES").is_some()
        || std::env::var_os("VK_LOADER_DRIVERS_SELECT").is_some()
}

/// Tell the Vulkan loader which manifests to load. Must run before this
/// process's first Vulkan instance: the loader reads the list as it builds
/// one, and a driver it has opened stays open for the life of the process.
///
/// Both spellings are set — `VK_DRIVER_FILES` is the loader's current name
/// (1.3.207 and later), `VK_ICD_FILENAMES` the one an older loader reads
/// (Ubuntu 22.04 ships 1.3.204). A loader that knows neither ignores the
/// variable and loads everything, which is the fallback every other failure
/// takes: not being understood breaks nothing.
pub fn apply(plan: &Plan) {
    let Some(drivers) = plan.restriction() else {
        report_fallback(plan);
        return;
    };
    let list = drivers
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(":");
    let _ = OWN_DRIVERS.set(list.clone());
    std::env::set_var("VK_DRIVER_FILES", &list);
    std::env::set_var("VK_ICD_FILENAMES", &list);
    let card = plan
        .card()
        .expect("a restriction is only made for a picked card");
    eprintln!(
        "kimo: GPU {} ({}) — loading {} of {} Vulkan drivers: {}",
        card.pci,
        vendor_name(card.vendor).unwrap_or("unknown vendor"),
        drivers.len(),
        plan.all.len(),
        manifest_names(drivers),
    );
}

/// One line for a pick that could not be honoured: every one of them is a
/// fallback, and a reader who picked a card should be able to find out why it
/// did not take.
fn report_fallback(plan: &Plan) {
    // Nothing to explain when no card was picked.
    if plan.saved.is_none() {
        return;
    }
    match &plan.status {
        Status::NotPresent(slot) => {
            eprintln!("kimo: GPU {slot} is not installed here; loading every Vulkan driver");
        }
        Status::Malformed(value) => {
            eprintln!(
                "kimo: GPU setting {value:?} is not a PCI address; loading every Vulkan driver"
            );
        }
        Status::NoDriver(card) => {
            eprintln!(
                "kimo: no Vulkan driver serves GPU {} ({}); loading every Vulkan driver",
                card.pci,
                vendor_name(card.vendor).unwrap_or("unknown vendor"),
            );
        }
        Status::Environment => {
            eprintln!(
                "kimo: VK_DRIVER_FILES is set in this session; leaving the Vulkan drivers to it"
            );
        }
        // A pick that drops nothing is honoured in effect: the same list loads.
        Status::Automatic | Status::Picked(_) => {}
    }
}

/// The manifest file names, for a log line or a page: `intel_icd.json,
/// intel_hasvk_icd.json`.
pub fn manifest_names(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// What Preferences → Graphics lists: the cards, the drivers, the cards' names,
/// and whether the session already chose the drivers itself.
#[derive(Clone, Debug)]
pub struct Scan {
    pub cards: Vec<Card>,
    pub icds: Vec<Icd>,
    pub names: Names,
    pub locked: bool,
}

/// The page's whole input, gathered on a worker thread (`Job::ScanGpus`): sysfs
/// and the manifest directories are filesystem work the UI thread must not do.
pub fn scan() -> Scan {
    let cards = cards();
    let names = names_for(&cards);
    Scan {
        icds: icds(),
        cards,
        names,
        locked: environment_lock(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(pci: &str, vendor: u32, device: u32) -> Card {
        Card {
            pci: pci.to_string(),
            vendor,
            device,
            driver: "driver".to_string(),
        }
    }

    fn icd(file: &str, vendor: Option<u32>, software: bool) -> Icd {
        Icd {
            path: PathBuf::from(format!("/usr/share/vulkan/icd.d/{file}")),
            library: file.to_string(),
            vendor,
            software,
        }
    }

    fn cards() -> Vec<Card> {
        vec![
            card("0000:03:00.0", 0x8086, 0xe20b),
            card("0000:0b:00.0", 0x10de, 0x1e84),
        ]
    }

    /// The box's four manifests in the order sysfs would give them.
    fn icds() -> Vec<Icd> {
        vec![
            icd("intel_hasvk_icd.json", Some(0x8086), false),
            icd("intel_icd.json", Some(0x8086), false),
            icd("lvp_icd.json", None, true),
            icd("nvidia_icd.json", Some(0x10de), false),
        ]
    }

    #[test]
    fn a_display_controller_with_a_rendering_driver_is_a_card() {
        let intel = "DRIVER=xe\nPCI_CLASS=30000\nPCI_ID=8086:E20B\nPCI_SUBSYS_ID=8086:1100\nPCI_SLOT_NAME=0000:03:00.0\n";
        let card = Card::from_uevent(intel).expect("a display controller");
        assert_eq!(card.pci, "0000:03:00.0");
        assert_eq!((card.vendor, card.device), (0x8086, 0xe20b));
        assert_eq!(card.driver, "xe");

        // An audio function of the same card is not a GPU.
        let audio = intel.replace("PCI_CLASS=30000", "PCI_CLASS=40300");
        assert!(Card::from_uevent(&audio).is_none());
        // Nor is the firmware framebuffer, which has no driver to draw with.
        let plain = intel.replace("DRIVER=xe", "DRIVER=simpledrm");
        assert!(Card::from_uevent(&plain).is_none());
        // A platform device (an ARM GPU) writes no PCI line and is not offered.
        let platform = "DRIVER=panfrost\n";
        assert!(Card::from_uevent(platform).is_none());
    }

    #[test]
    fn a_drivers_library_names_its_vendor() {
        assert_eq!(vendor_of("libvulkan_intel.so"), Some(0x8086));
        assert_eq!(vendor_of("/usr/lib/libvulkan_intel_hasvk.so"), Some(0x8086));
        assert_eq!(vendor_of("libGLX_nvidia.so.0"), Some(0x10de));
        assert_eq!(vendor_of("libvulkan_radeon.so"), Some(0x1002));
        assert_eq!(vendor_of("amdvlk64.so"), Some(0x1002));
        assert_eq!(vendor_of("libvulkan_lvp.so"), None);
        assert_eq!(vendor_of("acme_vk.so"), None);
    }

    #[test]
    fn a_manifest_names_the_library_it_loads() {
        let text = r#"{ "ICD": { "api_version": "1.4.354", "library_path": "libvulkan_intel.so" }, "file_format_version": "1.0.1" }"#;
        let parsed = parse_manifest(Path::new("/etc/vulkan/icd.d/intel_icd.json"), text)
            .expect("a manifest");
        assert_eq!(parsed.library, "libvulkan_intel.so");
        assert_eq!(parsed.vendor, Some(0x8086));
        assert!(!parsed.software);

        let nvidia = r#"{"ICD":{"library_path":"libGLX_nvidia.so.0"}}"#;
        let parsed = parse_manifest(Path::new("/etc/vulkan/icd.d/nvidia_icd.json"), nvidia)
            .expect("a manifest");
        assert_eq!(parsed.vendor, Some(0x10de));

        let lvp = r#"{"ICD":{"library_path":"libvulkan_lvp.so"}}"#;
        let parsed =
            parse_manifest(Path::new("/etc/vulkan/icd.d/lvp_icd.json"), lvp).expect("a manifest");
        assert!(parsed.software);
        assert_eq!(parsed.vendor, None);

        // Anything else is not a driver, and must not become a pathless one.
        assert!(parse_manifest(Path::new("/x.json"), "{ not json").is_none());
        assert!(parse_manifest(Path::new("/x.json"), r#"{"ICD":{"api_version":"1.0"}}"#).is_none());
        assert!(
            parse_manifest(Path::new("/x.json"), r#"{"file_format_version":"1.0.1"}"#).is_none()
        );
    }

    #[test]
    fn a_pick_loads_its_drivers_and_not_the_other_cards() {
        let icds = icds();
        let intel = plan(Some("0000:03:00.0"), &cards(), &icds, false);
        assert_eq!(intel.status, Status::Picked(cards()[0].clone()));
        assert_eq!(
            intel.drivers,
            vec![
                PathBuf::from("/usr/share/vulkan/icd.d/intel_hasvk_icd.json"),
                PathBuf::from("/usr/share/vulkan/icd.d/intel_icd.json"),
            ],
            "the Intel drivers, and not lavapipe or NVIDIA"
        );
        assert_eq!(intel.all.len(), 4);
        assert_eq!(intel.restriction(), Some(&intel.drivers[..]));

        // And the other way round: NVIDIA is one manifest, Intel's two go.
        let nvidia = plan(Some("0000:0b:00.0"), &cards(), &icds, false);
        assert_eq!(
            nvidia.drivers,
            vec![PathBuf::from("/usr/share/vulkan/icd.d/nvidia_icd.json")]
        );
    }

    #[test]
    fn a_driver_nothing_is_known_about_is_kept() {
        let mut icds = icds();
        let unknown = icd("/opt/acme/icd.d/acme_icd.json", None, false);
        icds.push(unknown.clone());
        let plan = plan(Some("0000:03:00.0"), &cards(), &icds, false);
        assert!(
            plan.drivers.contains(&unknown.path),
            "not being able to attribute a driver is no reason to drop it"
        );
    }

    #[test]
    fn a_pick_that_is_not_here_falls_back_to_every_driver() {
        let plan = plan(Some("0000:0c:00.0"), &cards(), &icds(), false);
        assert_eq!(plan.status, Status::NotPresent("0000:0c:00.0".to_string()));
        assert_eq!(plan.drivers, plan.all);
        assert_eq!(plan.restriction(), None);
    }

    #[test]
    fn a_pick_that_is_not_an_address_falls_back_too() {
        for saved in ["the nvidia one", "", "0000:03:00", "0000:03:00.8"] {
            let plan = plan(Some(saved), &cards(), &icds(), false);
            assert_eq!(plan.status, Status::Malformed(saved.to_string()));
            assert_eq!(plan.drivers, plan.all, "every driver still loads");
            assert_eq!(plan.restriction(), None);
        }
    }

    #[test]
    fn a_card_no_driver_serves_falls_back_to_every_driver() {
        let only_nvidia = vec![icd("nvidia_icd.json", Some(0x10de), false)];
        let served = plan(Some("0000:03:00.0"), &cards(), &only_nvidia, false);
        assert_eq!(served.status, Status::NoDriver(cards()[0].clone()));
        assert_eq!(served.drivers, served.all);
        assert_eq!(served.restriction(), None);

        // Nothing installed at all is the same answer, not an empty list.
        let empty = plan(Some("0000:03:00.0"), &cards(), &[], false);
        assert!(matches!(empty.status, Status::NoDriver(_)));
        assert!(empty.drivers.is_empty());
        assert_eq!(empty.restriction(), None);
    }

    #[test]
    fn the_sessions_own_driver_list_is_left_alone() {
        let plan = plan(Some("0000:03:00.0"), &cards(), &icds(), true);
        assert_eq!(plan.status, Status::Environment);
        assert_eq!(plan.drivers, plan.all);
        assert_eq!(plan.restriction(), None);
    }

    #[test]
    fn automatic_loads_every_driver() {
        let plan = plan(None, &cards(), &icds(), false);
        assert_eq!(plan.status, Status::Automatic);
        assert_eq!(plan.drivers, plan.all);
        assert_eq!(plan.drivers.len(), 4);
        assert_eq!(plan.restriction(), None);
        assert_eq!(plan.card(), None);
    }

    #[test]
    fn a_pick_that_drops_nothing_does_not_override_the_loader() {
        // One card, one driver: there is nothing to keep out, and the loader's
        // own list is the safer one (it may know a manifest this scan did not).
        let plan = plan(
            Some("0000:03:00.0"),
            &cards()[..1],
            &[icd("intel_icd.json", Some(0x8086), false)],
            false,
        );
        assert_eq!(plan.status, Status::Picked(cards()[0].clone()));
        assert_eq!(plan.drivers, plan.all);
        assert_eq!(plan.restriction(), None);
    }

    #[test]
    fn names_come_out_of_the_pci_database() {
        let text = concat!(
            "# PCI IDs\n",
            "8086  Intel Corporation\n",
            "\te20b  Battlemage G21 [Arc B580]\n",
            "\t\t8086 1100  Subsystem of a card\n",
            "10de  NVIDIA Corporation\n",
            "\t1e84  TU104 [GeForce RTX 2070 SUPER]\n",
            "C 03  Display controller\n",
            "\t00  VGA compatible controller\n",
        );
        let names = names_from(text, &cards());
        assert_eq!(names.vendor(0x8086), Some("Intel Corporation"));
        assert_eq!(
            names.device(0x8086, 0xe20b),
            Some("Battlemage G21 [Arc B580]")
        );
        assert_eq!(
            names.device(0x10de, 0x1e84),
            Some("TU104 [GeForce RTX 2070 SUPER]")
        );
        // The class list's entries are not devices of the vendor above it.
        assert_eq!(names.device(0x10de, 0x0300), None);
    }

    #[test]
    fn a_slot_is_compared_without_its_domain() {
        assert!(same_slot("0000:03:00.0", "0000:03:00.0"));
        assert!(same_slot("0000:03:00.0", "03:00.0"));
        assert!(same_slot("0000:0B:00.0", "0000:0b:00.0"));
        assert!(!same_slot("0000:03:00.0", "0000:0b:00.0"));
    }

    #[test]
    fn a_slot_has_to_look_like_one() {
        assert!(is_pci_slot("0000:03:00.0"));
        assert!(is_pci_slot("0000:0b:1f.7"));
        assert!(!is_pci_slot("0000:03:00"));
        assert!(!is_pci_slot("0000:03:00.8"));
        assert!(!is_pci_slot("nvidia"));
        assert!(!is_pci_slot(""));
        assert!(!is_pci_slot("0000:03:00.0 "));
    }
}
