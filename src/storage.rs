//! Local persistence: account configuration and per-account message caches,
//! stored as JSON under the XDG config/data directories.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::models::{Account, MailMessage};

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

pub fn load_accounts() -> Vec<Account> {
    let path = config_dir().join("accounts.json");
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// UI settings (`~/.config/kimo/settings.json`).
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// "system" | "light" | "dark"; anything else behaves as "system".
    #[serde(default)]
    pub theme: String,
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

pub fn save_accounts(accounts: &[Account]) -> std::io::Result<()> {
    let dir = config_dir();
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("accounts.json"),
        serde_json::to_string_pretty(accounts)?,
    )?;
    Ok(())
}

/// Messages cached for one folder. IMAP folders also remember their
/// UIDVALIDITY generation so a change invalidates stale UIDs.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct FolderCache {
    pub name: String,
    pub uidvalidity: Option<u32>,
    /// False until the first successful sync, so the UI can distinguish
    /// "not fetched yet" from "really empty".
    pub synced: bool,
    pub messages: Vec<MailMessage>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct AccountCache {
    pub folders: Vec<FolderCache>,
}

pub fn load_cache(account_id: &str) -> AccountCache {
    let path = data_dir().join("cache").join(format!("{account_id}.json"));
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_cache(account_id: &str, cache: &AccountCache) -> std::io::Result<()> {
    let dir = data_dir().join("cache");
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join(format!("{account_id}.json")),
        serde_json::to_string_pretty(cache)?,
    )?;
    Ok(())
}

pub fn delete_cache(account_id: &str) {
    let _ = fs::remove_file(data_dir().join("cache").join(format!("{account_id}.json")));
}

// ----- remote resource cache -------------------------------------------------

/// Disk cache for fetched remote mail resources (images, stylesheets),
/// keyed by the SHA-1 of the URL:
/// `~/.local/share/kimo/cache/resources/<sha1hex>.bin`.
fn resource_path(url: &str) -> PathBuf {
    let digest = sha1_smol::Sha1::from(url).digest().to_string();
    data_dir()
        .join("cache")
        .join("resources")
        .join(format!("{digest}.bin"))
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
            html_renderer: Some("blitz".into()),
            ui_font: Some("DejaVu Sans".into()),
            mono_font: Some("DejaVu Sans Mono".into()),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.theme, "dark");
        assert_eq!(back.html_renderer.as_deref(), Some("blitz"));
        assert_eq!(back.ui_font.as_deref(), Some("DejaVu Sans"));
        assert_eq!(back.mono_font.as_deref(), Some("DejaVu Sans Mono"));
        // settings.json with missing keys must still load.
        let missing: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(missing.theme, "");
        assert_eq!(missing.html_renderer, None);
        assert_eq!(missing.ui_font, None);
        assert_eq!(missing.mono_font, None);
        // settings.json written when the web view renderer still existed
        // carries a render_mode field; it is ignored, not an error.
        let legacy: Settings = serde_json::from_str(r#"{"theme":"light","render_mode":"web"}"#)
            .expect("unknown fields must be ignored");
        assert_eq!(legacy.theme, "light");
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

    #[test]
    fn cache_round_trip() {
        let mut cache = AccountCache::default();
        cache.folders.push(FolderCache {
            name: "INBOX".into(),
            uidvalidity: Some(7),
            synced: true,
            messages: vec![],
        });
        let json = serde_json::to_string(&cache).unwrap();
        let back: AccountCache = serde_json::from_str(&json).unwrap();
        assert_eq!(back.folders[0].name, "INBOX");
        assert_eq!(back.folders[0].uidvalidity, Some(7));
    }
}
