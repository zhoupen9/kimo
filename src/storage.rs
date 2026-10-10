//! Local persistence: account configuration and per-account message caches,
//! stored as JSON under the XDG config/data directories.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::models::{Account, MailMessage};

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

pub fn config_dir() -> PathBuf {
    match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("jimao"),
        _ => home().join(".config").join("jimao"),
    }
}

pub fn data_dir() -> PathBuf {
    match std::env::var("XDG_DATA_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("jimao"),
        _ => home().join(".local").join("share").join("jimao"),
    }
}

pub fn load_accounts() -> Vec<Account> {
    let path = config_dir().join("accounts.json");
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// UI settings (`~/.config/jimao/settings.json`).
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// "system" | "light" | "dark"; anything else behaves as "system".
    #[serde(default)]
    pub theme: String,
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
/// `~/.local/share/jimao/cache/resources/<sha1hex>.bin`.
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
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.theme, "dark");
        let missing: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(missing.theme, "");
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
