//! Local persistence paths and settings.
//!
//! Accounts and cached mail live in the SQLite database (`crate::db`), at
//! `~/.local/share/kimo/mail.db`. Before that they were JSON: the account list
//! in `~/.config/kimo/accounts.json` and one `<account-id>.json` per account
//! under the data directory. `db::import_legacy_json` reads those files once,
//! on the first start after the upgrade, and deletes them.
//!
//! What is left here: the XDG paths, `settings.json` (UI preferences), and the
//! on-disk cache for remote HTML-mail resources.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::models::MailMessage;

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// The application directory before the project was renamed to Kimo. Kept
/// only so an existing installation can be moved over; drop this and
/// [`migrate_legacy_dir`] once no installation can still predate the
/// rename.
const LEGACY_APP_DIR: &str = "jimao";

/// Move a pre-rename `jimao` directory to its `kimo` name, so the rename does
/// not orphan an account list or a message cache.
///
/// A `kimo` directory that already exists always wins, and failures are
/// ignored: the app then simply starts from defaults.
fn migrate_legacy_dir(current: &Path, legacy: &Path) {
    if current.exists() || !legacy.exists() {
        return;
    }
    if let Some(parent) = current.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::rename(legacy, current);
}

pub fn config_dir() -> PathBuf {
    let dir = match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("kimo"),
        _ => home().join(".config").join("kimo"),
    };
    migrate_legacy_dir(&dir, &dir.with_file_name(LEGACY_APP_DIR));
    dir
}

pub fn data_dir() -> PathBuf {
    let dir = match std::env::var("XDG_DATA_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("kimo"),
        _ => home().join(".local").join("share").join("kimo"),
    };
    migrate_legacy_dir(&dir, &dir.with_file_name(LEGACY_APP_DIR));
    dir
}

/// The one database holding every account and every cached message, plus its
/// `-wal`/`-shm` sidecars once it is open.
pub fn mail_db_path() -> PathBuf {
    data_dir().join("mail.db")
}

/// Where an exported mail is written when the desktop has no file dialog to
/// ask: `$HOME/Downloads`, the directory a desktop keeps downloads in (made on
/// the first export if it is not there yet). It is also the folder that dialog
/// is suggested to open on — the export asks the desktop where to put the file
/// (`crate::portal::save_file`) — so a user who presses Save without touching
/// the name gets the file they would have got with no dialog at all. A
/// downloaded attachment has this one destination: the card is a single click,
/// and Export is the action that asks where the file goes.
pub fn downloads_dir() -> PathBuf {
    home().join("Downloads")
}

/// The message cache directory of the pre-database layout:
/// `<account-id>.json` files, imported once by `crate::db` and then deleted.
/// The remote-resource cache lives in a subdirectory of it.
pub fn cache_dir() -> PathBuf {
    data_dir().join("cache")
}

/// UI settings (`~/.config/kimo/settings.json`).
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// "system" | "light" | "dark"; anything else behaves as "system".
    #[serde(default)]
    pub theme: String,
    /// The chosen accent, as its key in `theme::Accent::ALL`; absent — or a
    /// key no accent answers to — means Kimo's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    /// Which HTML body renderer to use: "egui" (the built-in widget
    /// renderer, the default) or "blitz" (full CSS layout, rasterized).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_renderer: Option<String>,
    /// Family name of the UI (proportional) font, as picked in
    /// Preferences → Appearance; absent means egui's built-in font.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_font: Option<String>,
    /// Family name of the fixed-width font used for monospace text; absent
    /// means egui's built-in monospace font.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mono_font: Option<String>,
    /// Family name of the CJK fallback face, as picked in Preferences →
    /// Appearance. Absent — or a name that is no longer installed — means the
    /// automatic choice: the first of `fonts::CJK_CANDIDATES` present, with the
    /// face the locale asks for (`CjkScript`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cjk_font: Option<String>,
    /// Whether the message list groups mail into conversations
    /// (`crate::threads`). Absent means the app's own default, which is on:
    /// threading is what the list does unless it is told otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threaded: Option<bool>,
    /// Whether Kimo syncs every account on a timer, and how many minutes apart
    /// its sweeps are (`KimoApp::pump_auto_sync`). Both absent mean the app's
    /// own defaults: off, and `AUTO_SYNC_MINUTES` between sweeps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_sync: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_sync_minutes: Option<u32>,
    /// Whether new mail raises a desktop notification
    /// (`crate::notify`). Absent means the app's own default, which is on:
    /// the feature is the point of the switch existing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notifications: Option<bool>,
    /// Whether the title bar's minimize button hides Kimo instead of
    /// minimising it, leaving the tray icon as the way back
    /// (`KimoApp::minimize_to_tray`). Absent means off: the window manager's
    /// minimize is the safe default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimize_to_tray: Option<bool>,
    /// Whether closing Kimo — the title bar's cross, or the compositor's own
    /// close request — hides it instead of quitting, leaving the tray menu's
    /// Quit as the way out (`KimoApp::close_to_tray`). Absent means off:
    /// closing quits, as it always has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_to_tray: Option<bool>,
    /// The PCI address of the graphics card Kimo draws with
    /// (`0000:03:00.0`), as picked in Preferences → Graphics; absent means the
    /// Vulkan loader's own choice. Read at start-up, before any Vulkan instance
    /// exists, so the drivers of the cards it is not are never loaded
    /// (`crate::gpus`).
    ///
    /// Read leniently: a value of the wrong JSON type is treated as absent
    /// rather than as a broken file — one bad key must not lose every other
    /// preference — and a string that names no card falls back to every driver
    /// in `crate::gpus::plan`.
    #[serde(
        default,
        deserialize_with = "lenient_gpu",
        skip_serializing_if = "Option::is_none"
    )]
    pub gpu: Option<String>,
}

/// A `gpu` value that is not a string is no pick at all, and only this key is
/// lost. Without this reader a file edited to `"gpu": 42` would fail to parse
/// whole, and `load_settings` would hand back every default instead of the
/// preferences around it.
fn lenient_gpu<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| match value {
        serde_json::Value::String(text) => Some(text),
        _ => None,
    }))
}

pub fn load_settings() -> Settings {
    fs::read_to_string(config_dir().join("settings.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_settings(settings: &Settings) -> std::io::Result<()> {
    let dir = config_dir();
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("settings.json"),
        serde_json::to_string_pretty(settings)?,
    )?;
    Ok(())
}

/// One folder's mail, as the app keeps it in memory. IMAP folders also
/// remember their UIDVALIDITY generation so a change invalidates stale UIDs.
#[derive(Default, Clone)]
pub struct Folder {
    pub name: String,
    pub uidvalidity: Option<u32>,
    /// False until the first successful sync, so the UI can distinguish
    /// "not fetched yet" from "really empty".
    pub synced: bool,
    /// Whether the server listed this folder the last time it was asked. A
    /// folder that dropped out of the list keeps its mail — the only copy of it
    /// this machine has — so the column keeps the row too, and this is what
    /// lets it say that the server no longer has it.
    pub listed: bool,
    /// Whether the folder's marks say the server has mail below the oldest this
    /// machine holds — what the message list's `Load older mail` button is
    /// drawn from. A fact about the server as of the last time it was asked,
    /// like `listed`: `db::load_mailboxes` derives it at startup, and the jobs
    /// that ask the server report it as they learn it.
    pub can_load_older: bool,
    pub messages: Vec<MailMessage>,
}

/// Every folder of one account, loaded from the database at startup.
#[derive(Default, Clone)]
pub struct Mailbox {
    pub folders: Vec<Folder>,
}

// ----- remote resource cache -------------------------------------------------

/// Disk cache for fetched remote mail resources (images, stylesheets),
/// keyed by the SHA-1 of the URL:
/// `~/.local/share/kimo/cache/resources/<sha1hex>.bin`.
fn resource_path(url: &str) -> PathBuf {
    let digest = sha1_smol::Sha1::from(url).digest().to_string();
    cache_dir().join("resources").join(format!("{digest}.bin"))
}

pub fn load_resource(url: &str) -> Option<Vec<u8>> {
    fs::read(resource_path(url)).ok()
}

pub fn save_resource(url: &str, bytes: &[u8]) {
    let path = resource_path(url);
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(path, bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_default() {
        let s = Settings {
            theme: "dark".into(),
            accent: Some("teal".into()),
            html_renderer: Some("blitz".into()),
            ui_font: Some("DejaVu Sans".into()),
            threaded: Some(false),
            mono_font: Some("DejaVu Sans Mono".into()),
            cjk_font: Some("Noto Sans CJK SC".into()),
            auto_sync: Some(true),
            auto_sync_minutes: Some(30),
            notifications: Some(false),
            minimize_to_tray: Some(true),
            close_to_tray: Some(true),
            gpu: Some("0000:03:00.0".into()),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.theme, "dark");
        assert_eq!(back.accent.as_deref(), Some("teal"));
        assert_eq!(back.html_renderer.as_deref(), Some("blitz"));
        assert_eq!(back.ui_font.as_deref(), Some("DejaVu Sans"));
        assert_eq!(back.threaded, Some(false));
        assert_eq!(back.mono_font.as_deref(), Some("DejaVu Sans Mono"));
        assert_eq!(back.cjk_font.as_deref(), Some("Noto Sans CJK SC"));
        assert_eq!(back.auto_sync, Some(true));
        assert_eq!(back.auto_sync_minutes, Some(30));
        assert_eq!(back.notifications, Some(false));
        assert_eq!(back.minimize_to_tray, Some(true));
        assert_eq!(back.close_to_tray, Some(true));
        assert_eq!(back.gpu.as_deref(), Some("0000:03:00.0"));
        // settings.json with missing keys must still load.
        let missing: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(missing.theme, "");
        assert_eq!(missing.accent, None, "absent means Kimo's own accent");
        assert_eq!(missing.html_renderer, None);
        assert_eq!(missing.ui_font, None);
        assert_eq!(missing.mono_font, None);
        assert_eq!(
            missing.cjk_font, None,
            "absent means the automatic fallback"
        );
        assert_eq!(missing.threaded, None, "absent means the app's default");
        assert_eq!(missing.auto_sync, None, "absent means the app's default");
        assert_eq!(missing.auto_sync_minutes, None);
        assert_eq!(
            missing.notifications, None,
            "absent means the app's default, which is to announce"
        );
        assert_eq!(
            missing.minimize_to_tray, None,
            "absent means the app's default, which is the normal minimize"
        );
        assert_eq!(
            missing.close_to_tray, None,
            "absent means the app's default, which is to quit on close"
        );
        assert_eq!(
            missing.gpu, None,
            "absent means the Vulkan loader's own choice"
        );
        // A key a version wrote and no setting stands behind any more — the
        // web view renderer left a `render_mode` behind — is ignored rather
        // than refused, so the file still loads, and it goes when the file is
        // next written rather than being kept as something that does nothing.
        let legacy: Settings = serde_json::from_str(r#"{"theme":"light","render_mode":"web"}"#)
            .expect("unknown fields must be ignored");
        assert_eq!(legacy.theme, "light");
        assert!(!serde_json::to_string(&legacy)
            .unwrap()
            .contains("render_mode"));
    }

    #[test]
    fn a_gpu_of_the_wrong_type_keeps_the_other_settings() {
        // One bad key costs that key alone: before the lenient reader, a `gpu`
        // that was not a string failed the whole file and every preference fell
        // back to its default.
        let settings: Settings =
            serde_json::from_str(r#"{"theme":"dark","gpu":42,"auto_sync":true}"#)
                .expect("a non-string gpu is no reason to refuse the file");
        assert_eq!(settings.gpu, None);
        assert_eq!(settings.theme, "dark");
        assert_eq!(settings.auto_sync, Some(true));

        // A string is kept as written, even one no card answers to: which value
        // was ignored is `crate::gpus`'s fallback to say.
        let settings: Settings = serde_json::from_str(r#"{"gpu":"the nvidia one"}"#).unwrap();
        assert_eq!(settings.gpu.as_deref(), Some("the nvidia one"));
        let settings: Settings = serde_json::from_str(r#"{"gpu":null}"#).unwrap();
        assert_eq!(settings.gpu, None);
    }

    #[test]
    fn legacy_jimao_directory_is_migrated() {
        let root = std::env::temp_dir().join(format!("kimo-storage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let legacy = root.join("jimao");
        let current = root.join("kimo");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("accounts.json"), "[]").unwrap();

        migrate_legacy_dir(&current, &legacy);
        assert!(current.join("accounts.json").exists(), "content moved over");
        assert!(!legacy.exists(), "legacy directory is gone");

        // Once a `kimo` directory exists it wins, and the legacy one is left be.
        fs::create_dir_all(&legacy).unwrap();
        migrate_legacy_dir(&current, &legacy);
        assert!(legacy.exists(), "an existing kimo directory wins");

        let _ = fs::remove_dir_all(&root);
    }
}
